// Request context, routing and shared helpers — the Rust equivalent of the
// Go forge's server.go. Panics inside a handler are caught per request and
// answered 500 with just {"message": ...}, like the Node forge's catch-all.

use crate::git;
use crate::graphql;
use crate::http::{query_get, HttpRequest, HttpResponse, ResponseBody};
use crate::json::{body_bool, body_string, body_string_list, parse_body_object, Json};
use crate::mcp;
use crate::rest;
use crate::store::{RepoState, Store, VIEWER_LOGIN};
use crate::util::{js_number, path_unescape, trim_space};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Forge {
    pub store: Arc<Store>,
    pub token: String,
}

pub struct Ctx<'a> {
    pub store: &'a Arc<Store>,
    pub token: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a HashMap<String, Vec<String>>,
    pub body: &'a [u8],
    pub origin: String,
    pub is_head: bool,
    pub auth: &'a str,
    pub git_protocol: &'a str,
}

impl<'a> Ctx<'a> {
    pub fn json_resp(&self, status: u16, body: ResponseBody, extra: &[(&str, &str)]) -> HttpResponse {
        let mut headers: Vec<(String, String)> =
            vec![("x-github-request-id".to_string(), self.store.next_req_id())];
        for (k, v) in extra {
            headers.push((k.to_string(), v.to_string()));
        }
        let mut resp = HttpResponse::new(status, body);
        resp.headers = headers;
        resp
    }

    pub fn json_bytes(&self, status: u16, body: Vec<u8>, extra: &[(&str, &str)]) -> HttpResponse {
        self.json_resp(status, ResponseBody::Owned(body), extra)
    }

    pub fn json_shared(&self, status: u16, body: Arc<Vec<u8>>, extra: &[(&str, &str)]) -> HttpResponse {
        self.json_resp(status, ResponseBody::Shared(body), extra)
    }

    pub fn err_json(&self, status: u16, message: &str) -> HttpResponse {
        let mut w: Vec<u8> = Vec::with_capacity(96);
        w.extend_from_slice(b"{\"message\":");
        crate::json::push_json_string(&mut w, message);
        w.extend_from_slice(b",\"documentation_url\":\"https://example.invalid/docs\"}");
        self.json_bytes(status, w, &[])
    }

    pub fn authorized(&self) -> bool {
        let header = self.auth;
        let Some(presented) = header.strip_prefix("Bearer ") else {
            return false;
        };
        let presented = trim_space(presented);
        if presented.is_empty() {
            return false;
        }
        if self.token.is_empty() {
            return true;
        }
        presented == self.token
    }

    pub fn parse_body(&self) -> Option<Vec<(String, Json)>> {
        parse_body_object(self.body)
    }

    pub fn page_params(&self) -> (i64, i64) {
        let mut per_page = 30i64;
        if let Some(raw) = self.query.get("per_page").and_then(|v| v.first()) {
            if let Some(f) = js_number(raw) {
                let mut f = f;
                if f == 0.0 {
                    f = 30.0;
                }
                per_page = (f.max(1.0).min(100.0)) as i64;
            }
        }
        let mut page = 1i64;
        if let Some(raw) = self.query.get("page").and_then(|v| v.first()) {
            if let Some(f) = js_number(raw) {
                let mut f = f;
                if f == 0.0 {
                    f = 1.0;
                }
                page = (f.max(1.0)) as i64;
            }
        }
        (per_page, page)
    }

    /// Slice items and build the Link header value, mirroring Node.
    pub fn paginate(&self, total: usize, base_path: &str) -> (usize, usize, String) {
        let (per_page, page) = self.page_params();
        let per_page = per_page as usize;
        let page = page as usize;
        let mut start = (page - 1) * per_page;
        let mut end = start + per_page;
        if end > total {
            end = total;
        }
        if start > total {
            start = total;
            end = total;
        }
        let mut link = String::new();
        if start + per_page < total {
            link.push_str(&format!(
                "<{}?per_page={}&page={}>; rel=\"next\"",
                format!("{}{}", self.origin, base_path),
                per_page,
                page + 1
            ));
            if page > 1 {
                link.push_str(&format!(
                    ", <{}?per_page={}&page={}>; rel=\"prev\"",
                    format!("{}{}", self.origin, base_path),
                    per_page,
                    page - 1
                ));
            }
        } else if page > 1 {
            link.push_str(&format!(
                "<{}?per_page={}&page={}>; rel=\"prev\"",
                format!("{}{}", self.origin, base_path),
                per_page,
                page - 1
            ));
        }
        (start, end, link)
    }

    pub fn repo_or_404(&self, owner: &str, name: &str) -> Option<Arc<RepoState>> {
        if owner != VIEWER_LOGIN {
            return None;
        }
        self.store.get_repo(owner, name)
    }
}

impl Forge {
    pub fn new(token: String) -> Forge {
        Forge { store: Arc::new(Store::new()), token }
    }

    pub fn handle(&self, req: &HttpRequest) -> HttpResponse {
        let mut origin = format!("http://{}", req.host);
        if origin == "http://" {
            origin = "http://127.0.0.1".to_string();
        }
        let ctx = Ctx {
            store: &self.store,
            token: &self.token,
            method: &req.method,
            path: &req.path,
            query: &req.query,
            body: &req.body,
            origin,
            is_head: req.method == "HEAD",
            auth: &req.auth,
            git_protocol: &req.git_protocol,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.route(&ctx)));
        let mut resp = match result {
            Ok(r) => r,
            Err(e) => {
                let msg: String = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "Internal error".to_string()
                };
                let mut w: Vec<u8> = Vec::with_capacity(64);
                w.extend_from_slice(b"{\"message\":");
                crate::json::push_json_string(&mut w, &msg);
                w.extend_from_slice(b"}");
                ctx.json_bytes(500, w, &[])
            }
        };
        if ctx.is_head {
            resp.head_only = true;
        }
        resp
    }

    fn route(&self, c: &Ctx) -> HttpResponse {
        if c.method == "GET" && c.path == "/healthz" {
            return c.json_bytes(200, b"{\"ok\":true}".to_vec(), &[]);
        }
        if c.path == "/api/graphql" && c.method == "POST" {
            return graphql::handle_graphql(c);
        }
        if c.path == "/mcp" {
            return mcp::handle_mcp(c);
        }
        if c.path.starts_with("/api/v3") {
            if let Some(resp) = rest::handle_rest(c) {
                return resp;
            }
        }
        if let Some(resp) = git::handle_git(c) {
            return resp;
        }
        c.err_json(404, "Not Found")
    }
}

// --- shared helpers used by the handlers ---

pub fn split_path(path: &str) -> Vec<String> {
    path.split('/').filter(|p| !p.is_empty()).map(|p| p.to_string()).collect()
}

/// Decodes one path segment a second time; panics on failure so the
/// router's catch-all answers 500, exactly like the Node forge.
pub fn must_decode(s: &str) -> String {
    match path_unescape(s) {
        Some(out) => out,
        None => panic!("bad escape"),
    }
}

pub fn body_str(fields: &[(String, Json)], key: &str) -> String {
    body_string(fields, key)
}

pub fn body_b(fields: &[(String, Json)], key: &str) -> bool {
    body_bool(fields, key)
}

pub fn body_list(fields: &[(String, Json)], key: &str) -> Option<Vec<String>> {
    body_string_list(fields, key)
}

pub fn query_get_str<'a>(c: &'a Ctx, key: &str) -> &'a str {
    query_get(c.query, key)
}

/// Sort issues by number, honouring sort=created&direction=desc.
pub fn sort_issues_list(
    mut list: Vec<Arc<crate::store::Issue>>,
    sort_field: &str,
    direction: &str,
) -> Vec<Arc<crate::store::Issue>> {
    if sort_field == "created" || sort_field.is_empty() {
        if direction == "desc" {
            list.sort_by(|a, b| b.number.cmp(&a.number));
        } else {
            list.sort_by(|a, b| a.number.cmp(&b.number));
        }
    }
    list
}
