// MCP (JSON-RPC 2.0 over Streamable HTTP; plain JSON answers, no SSE),
// with the same tool definitions and behaviour as the Go forge.

use crate::http::HttpResponse;
use crate::json::{to_json_bytes, write_json, Json, Parser};
use crate::server::Ctx;
use crate::store::{
    pr_json, repo_json, sorted_issues, sorted_pulls, RepoState, Store, VIEWER_LOGIN,
};
use std::sync::Arc;

const MCP_TOOLS_JSON: &str = r#"[{"name":"create_repo","description":"Create a repository owned by the authenticated user.","inputSchema":{"type":"object","properties":{"name":{"type":"string","description":"Repository name"},"description":{"type":"string","description":"Repository description"},"private":{"type":"boolean","description":"Whether the repository is private"}},"required":["name"]}},{"name":"get_repo","description":"Get a repository by owner and name.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"}},"required":["owner","repo"]}},{"name":"create_issue","description":"Create an issue in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"title":{"type":"string","description":"Issue title"},"body":{"type":"string","description":"Issue body"}},"required":["owner","repo","title"]}},{"name":"list_issues","description":"List issues in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"state":{"type":"string","description":"open, closed or all"}},"required":["owner","repo"]}},{"name":"create_pull_request","description":"Create a pull request in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"title":{"type":"string","description":"Pull request title"},"body":{"type":"string","description":"Pull request body"},"head":{"type":"string","description":"Head branch name"},"base":{"type":"string","description":"Base branch name"}},"required":["owner","repo","title","head","base"]}},{"name":"merge_pull_request","description":"Merge a pull request with a squash merge.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"pull_number":{"type":"integer","description":"Pull request number"}},"required":["owner","repo","pull_number"]}},{"name":"list_pull_requests","description":"List pull requests in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"state":{"type":"string","description":"open, closed or all"}},"required":["owner","repo"]}}]"#;

const MCP_TOOL_NAMES: [&str; 7] = [
    "create_repo",
    "get_repo",
    "create_issue",
    "list_issues",
    "create_pull_request",
    "merge_pull_request",
    "list_pull_requests",
];

fn mcp_tool_required(tool: &str) -> &'static [&'static str] {
    match tool {
        "create_repo" => &["name"],
        "get_repo" => &["owner", "repo"],
        "create_issue" => &["owner", "repo", "title"],
        "list_issues" => &["owner", "repo"],
        "create_pull_request" => &["owner", "repo", "title", "head", "base"],
        "merge_pull_request" => &["owner", "repo", "pull_number"],
        "list_pull_requests" => &["owner", "repo"],
        _ => &[],
    }
}

fn arg_str(args: &[(String, Json)], key: &str) -> String {
    match args.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
        Some(Json::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

fn arg_bool(args: &[(String, Json)], key: &str) -> bool {
    matches!(
        args.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        Some(Json::Bool(true))
    )
}

fn mcp_repo_args(args: &[(String, Json)]) -> (String, String) {
    let mut owner = arg_str(args, "owner");
    let mut repo = arg_str(args, "repo");
    if owner.is_empty() || repo.is_empty() {
        let full = arg_str(args, "full_name");
        if let Some(slash) = full.find('/') {
            if slash > 0 {
                owner = full[..slash].to_string();
                repo = full[slash + 1..].to_string();
            }
        }
    }
    (owner, repo)
}

/// A tool result: content[0].text is the JSON string of the value,
/// structuredContent is the value itself.
fn mcp_tool_result_json(value_json: &[u8]) -> Json {
    let text = String::from_utf8_lossy(value_json).into_owned();
    let structured = Parser::new(value_json).parse_document().unwrap_or(Json::Null);
    Json::Obj(vec![
        (
            "content".to_string(),
            Json::Arr(vec![Json::Obj(vec![
                ("type".to_string(), Json::Str("text".to_string())),
                ("text".to_string(), Json::Str(text)),
            ])]),
        ),
        ("structuredContent".to_string(), structured),
    ])
}

fn mcp_tool_error(message: &str) -> Json {
    Json::Obj(vec![
        (
            "content".to_string(),
            Json::Arr(vec![Json::Obj(vec![
                ("type".to_string(), Json::Str("text".to_string())),
                ("text".to_string(), Json::Str(message.to_string())),
            ])]),
        ),
        ("isError".to_string(), Json::Bool(true)),
    ])
}

fn mcp_missing_arg(args: &[(String, Json)], tool: &str) -> Option<String> {
    for required in mcp_tool_required(tool) {
        let v = args.iter().find(|(k, _)| k == required).map(|(_, v)| v);
        let missing = match v {
            None => true,
            Some(Json::Null) => true,
            Some(Json::Str(s)) => s.is_empty(),
            _ => false,
        };
        if missing {
            if *required == "owner" || *required == "repo" {
                let full = arg_str(args, "full_name");
                if full.contains('/') {
                    continue;
                }
            }
            return Some(required.to_string());
        }
    }
    None
}

fn name_valid(name: &str) -> bool {
    if name.is_empty() || name.len() > 100 {
        return false;
    }
    let mut chars = name.chars();
    if !chars.next().unwrap().is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn run_mcp_tool(store: &Arc<Store>, name: &str, args: &[(String, Json)]) -> Json {
    if !MCP_TOOL_NAMES.contains(&name) {
        return mcp_tool_error(&format!("unknown tool {}", name));
    }
    if let Some(missing) = mcp_missing_arg(args, name) {
        return mcp_tool_error(&format!("missing required argument: {}", missing));
    }
    if name == "create_repo" {
        let repo_name = arg_str(args, "name");
        if !name_valid(&repo_name) || repo_name.ends_with(".git") {
            return mcp_tool_error("invalid repository name");
        }
        if store.get_repo(VIEWER_LOGIN, &repo_name).is_some() {
            return mcp_tool_error("name already exists on this account");
        }
        let description = arg_str(args, "description");
        let is_private = arg_bool(args, "private");
        let repo = store.create_repo(VIEWER_LOGIN, &repo_name, is_private, &description);
        return match repo {
            Some(r) => mcp_tool_result_json(&repo_json(&r)),
            None => mcp_tool_error("name already exists on this account"),
        };
    }
    let (owner, repo_name) = mcp_repo_args(args);
    let repo: Option<Arc<RepoState>> = if owner == VIEWER_LOGIN {
        store.get_repo(&owner, &repo_name)
    } else {
        None
    };
    let repo = match repo {
        Some(r) => r,
        None => return mcp_tool_error("repository not found"),
    };
    match name {
        "get_repo" => mcp_tool_result_json(&repo_json(&repo)),
        "create_issue" => {
            let title = arg_str(args, "title");
            let body = arg_str(args, "body");
            match store.create_issue(&repo.owner, &repo.name, &title, &body, Vec::new()) {
                Some(iss) => mcp_tool_result_json(&iss.cached),
                None => mcp_tool_error("repository not found"),
            }
        }
        "list_issues" => {
            let mut state = arg_str(args, "state");
            if state.is_empty() {
                state = "open".to_string();
            }
            let mut list = sorted_issues(&repo);
            if state != "all" {
                list.retain(|iss| iss.state == state);
            }
            let mut w: Vec<u8> = Vec::new();
            w.push(b'[');
            for (i, iss) in list.iter().enumerate() {
                if i > 0 {
                    w.push(b',');
                }
                w.extend_from_slice(&iss.cached);
            }
            w.push(b']');
            mcp_tool_result_json(&w)
        }
        "create_pull_request" => {
            let head = arg_str(args, "head");
            let base = arg_str(args, "base");
            if !repo.refs.map.contains_key(&format!("refs/heads/{}", head))
                || !repo.refs.map.contains_key(&format!("refs/heads/{}", base))
            {
                return mcp_tool_error("head or base branch does not exist");
            }
            let title = arg_str(args, "title");
            let body = arg_str(args, "body");
            match store.create_pull_request(&repo.owner, &repo.name, &title, &body, &head, &base) {
                Some(pr) => {
                    let r = store.get_repo(&repo.owner, &repo.name).unwrap_or(repo);
                    mcp_tool_result_json(&pr_json(store, &r, &pr))
                }
                None => mcp_tool_error("could not create pull request"),
            }
        }
        "merge_pull_request" => {
            let number = match args.iter().find(|(k, _)| k == "pull_number").map(|(_, v)| v) {
                Some(Json::Num(n)) => *n as i64,
                _ => 0,
            };
            let pr = match repo.pulls.get(&number) {
                Some(p) => p.clone(),
                None => return mcp_tool_error("pull request not found"),
            };
            let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                let cur = match r.pulls.get(&number) {
                    Some(p) => p.clone(),
                    None => return (None, Err("not-found")),
                };
                match Store::merge_pull_request(store, r, &cur) {
                    Ok((nr, sha)) => (Some(nr), Ok(sha)),
                    Err(st) => (None, Err(st)),
                }
            });
            match out {
                Some(Ok(sha)) => {
                    let mut w: Vec<u8> = Vec::new();
                    w.extend_from_slice(b"{\"merged\":true,\"sha\":");
                    crate::json::push_json_string(&mut w, &sha);
                    w.extend_from_slice(b",\"message\":\"Pull request successfully merged\"}");
                    mcp_tool_result_json(&w)
                }
                Some(Err("already")) => mcp_tool_error("pull request is already merged"),
                Some(Err("conflict")) => mcp_tool_error("merge conflict"),
                _ => mcp_tool_error("pull request not found"),
            }
        }
        "list_pull_requests" => {
            let mut state = arg_str(args, "state");
            if state.is_empty() {
                state = "open".to_string();
            }
            let mut list = sorted_pulls(&repo);
            if state != "all" {
                list.retain(|pr| pr.state == state);
            }
            let mut w: Vec<u8> = Vec::new();
            w.push(b'[');
            for (i, pr) in list.iter().enumerate() {
                if i > 0 {
                    w.push(b',');
                }
                w.extend_from_slice(&pr_json(store, &repo, pr));
            }
            w.push(b']');
            mcp_tool_result_json(&w)
        }
        _ => mcp_tool_error(&format!("unknown tool {}", name)),
    }
}

fn mcp_error_obj(code: i64, message: &str) -> Json {
    Json::Obj(vec![(
        "error".to_string(),
        Json::Obj(vec![
            ("code".to_string(), Json::Num(code as f64)),
            ("message".to_string(), Json::Str(message.to_string())),
        ]),
    )])
}

fn mcp_reply(c: &Ctx, id: &Json, value: &Json) -> HttpResponse {
    let mut w: Vec<u8> = Vec::new();
    w.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
    write_json(&mut w, id);
    w.push(b',');
    // value's keys are appended after id (result or error).
    if let Json::Obj(fields) = value {
        for (i, (k, v)) in fields.iter().enumerate() {
            if i > 0 {
                w.push(b',');
            }
            crate::json::push_json_string(&mut w, k);
            w.push(b':');
            write_json(&mut w, v);
        }
    }
    w.push(b'}');
    c.json_bytes(200, w, &[])
}

pub fn handle_mcp(c: &Ctx) -> HttpResponse {
    if c.method == "DELETE" {
        let mut resp = HttpResponse::new(200, crate::http::ResponseBody::Owned(Vec::new()));
        resp.headers = vec![("x-github-request-id".to_string(), c.store.next_req_id())];
        resp.no_content_type = true;
        return resp;
    }
    if c.method != "POST" {
        return c.err_json(404, "Not Found");
    }
    if !c.authorized() {
        return c.err_json(401, "Requires authentication");
    }
    let body = match Parser::new(c.body).parse_document() {
        Ok(v) => v,
        Err(()) => {
            let mut resp = HttpResponse::new(
                400,
                crate::http::ResponseBody::Owned(
                    b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}"
                        .to_vec(),
                ),
            );
            resp.headers = vec![("x-github-request-id".to_string(), c.store.next_req_id())];
            return resp;
        }
    };
    let Json::Obj(rpc) = &body else {
        let mut resp = HttpResponse::new(
            400,
            crate::http::ResponseBody::Owned(
                b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"Invalid Request\"}}"
                    .to_vec(),
            ),
        );
        resp.headers = vec![("x-github-request-id".to_string(), c.store.next_req_id())];
        return resp;
    };
    let id = rpc.iter().find(|(k, _)| k == "id").map(|(_, v)| v.clone());
    let method = match rpc.iter().find(|(k, _)| k == "method").map(|(_, v)| v) {
        Some(Json::Str(s)) => s.clone(),
        _ => String::new(),
    };
    // A notification (no id) gets a 202 with no body.
    let Some(id) = id else {
        let mut resp = HttpResponse::new(202, crate::http::ResponseBody::Owned(Vec::new()));
        resp.headers = vec![("x-github-request-id".to_string(), c.store.next_req_id())];
        resp.no_content_type = true;
        return resp;
    };
    let params: Vec<(String, Json)> = match rpc.iter().find(|(k, _)| k == "params").map(|(_, v)| v) {
        Some(Json::Obj(fields)) => fields.clone(),
        _ => Vec::new(),
    };
    let param_str = |key: &str| -> String {
        match params.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
            Some(Json::Str(s)) => s.clone(),
            _ => String::new(),
        }
    };

    if method == "server/discover" {
        let result = Json::Obj(vec![
            (
                "supportedVersions".to_string(),
                Json::Arr(vec![Json::Str("2026-07-28".to_string()), Json::Str("2025-11-25".to_string())]),
            ),
            ("protocolVersion".to_string(), Json::Str("2026-07-28".to_string())),
            (
                "capabilities".to_string(),
                Json::Obj(vec![("tools".to_string(), Json::Obj(vec![]))]),
            ),
            (
                "serverInfo".to_string(),
                Json::Obj(vec![
                    ("name".to_string(), Json::Str(crate::store::SERVER_NAME.to_string())),
                    ("version".to_string(), Json::Str(crate::store::SERVER_VERSION.to_string())),
                ]),
            ),
            (
                "_meta".to_string(),
                Json::Obj(vec![(
                    "io.modelcontextprotocol/serverInfo".to_string(),
                    Json::Obj(vec![
                        ("name".to_string(), Json::Str(crate::store::SERVER_NAME.to_string())),
                        ("version".to_string(), Json::Str(crate::store::SERVER_VERSION.to_string())),
                    ]),
                )]),
            ),
        ]);
        let out = Json::Obj(vec![("result".to_string(), result)]);
        return mcp_reply(c, &id, &out);
    }
    if method == "initialize" {
        let requested = param_str("protocolVersion");
        let chosen = match requested.as_str() {
            "2026-07-28" | "2025-11-25" | "2025-06-18" | "2025-03-26" | "2024-11-05" => requested,
            _ => "2026-07-28".to_string(),
        };
        let result = Json::Obj(vec![
            ("protocolVersion".to_string(), Json::Str(chosen)),
            (
                "capabilities".to_string(),
                Json::Obj(vec![("tools".to_string(), Json::Obj(vec![]))]),
            ),
            (
                "serverInfo".to_string(),
                Json::Obj(vec![
                    ("name".to_string(), Json::Str(crate::store::SERVER_NAME.to_string())),
                    ("version".to_string(), Json::Str(crate::store::SERVER_VERSION.to_string())),
                ]),
            ),
        ]);
        let out = Json::Obj(vec![("result".to_string(), result)]);
        return mcp_reply(c, &id, &out);
    }
    if method == "tools/list" {
        // The tools value is a pre-serialised constant; embed it verbatim.
        let mut w: Vec<u8> = Vec::new();
        w.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
        write_json(&mut w, &id);
        w.extend_from_slice(b",\"result\":{\"tools\":");
        w.extend_from_slice(MCP_TOOLS_JSON.as_bytes());
        w.extend_from_slice(b"}}");
        return c.json_bytes(200, w, &[]);
    }
    if method == "tools/call" {
        let tool_name = param_str("name");
        let args: Vec<(String, Json)> = match params.iter().find(|(k, _)| k == "arguments").map(|(_, v)| v) {
            Some(Json::Obj(fields)) => fields.clone(),
            _ => Vec::new(),
        };
        if !MCP_TOOL_NAMES.contains(&tool_name.as_str()) {
            let out = mcp_error_obj(-32601, &format!("unknown tool {}", tool_name));
            return mcp_reply(c, &id, &out);
        }
        if let Some(missing) = mcp_missing_arg(&args, &tool_name) {
            let out = mcp_error_obj(-32602, &format!("missing required argument: {}", missing));
            return mcp_reply(c, &id, &out);
        }
        let result = run_mcp_tool(c.store, &tool_name, &args);
        let out = Json::Obj(vec![("result".to_string(), result)]);
        return mcp_reply(c, &id, &out);
    }
    let out = mcp_error_obj(-32601, &format!("method not found: {}", method));
    mcp_reply(c, &id, &out)
}

// Keep the import used (to_json_bytes is used by mcp_tool_result_json's
// caller chain indirectly through Parser; silence otherwise).
#[allow(dead_code)]
fn _unused(_: fn(&Json) -> Vec<u8>) {
    let _ = to_json_bytes;
}
