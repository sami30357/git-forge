// REST v3 handlers, mirroring the Go forge's routing, statuses, validation
// messages and response shapes. Mutations run inside store.mutate_repo so a
// repo's read-modify-swap is atomic per repo.

use crate::gitobj::{
    delete_path, diff_trees, get_path, list_dir, parse_commit, set_path, ObjType,
};
use crate::http::{query_get, HttpResponse, ResponseBody};
use crate::json::Json;
use crate::server::{body_b, body_list, body_str, must_decode, split_path, sort_issues_list, Ctx};
use crate::store::{
    comment_json, label_json, new_issue, pr_json, refresh_issue, repo_json, sorted_issues,
    sorted_pulls, user_json, Issue, Label, PullRequest, RepoState, Store, VIEWER_EMAIL,
    VIEWER_LOGIN, VIEWER_NAME,
};
use crate::util::{b64decode, b64encode, iso_of, is_oid, js_number, to_int_number};
use std::collections::HashMap;
use std::sync::Arc;

fn name_valid(name: &str) -> bool {
    // ^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$
    if name.is_empty() || name.len() > 100 {
        return false;
    }
    let mut chars = name.chars();
    if !chars.next().unwrap().is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

pub fn handle_rest(c: &Ctx) -> Option<HttpResponse> {
    let seg = split_path(c.path);
    if seg.len() < 2 || seg[0] != "api" || seg[1] != "v3" {
        return None;
    }
    let rest: Vec<String> = seg[2..].to_vec();
    let method = c.method;

    if !c.authorized() {
        return Some(c.err_json(401, "Requires authentication"));
    }

    // GET /api/v3/
    if rest.is_empty() {
        if method != "GET" {
            return Some(c.err_json(404, "Not Found"));
        }
        return Some(c.json_bytes(
            200,
            b"{}".to_vec(),
            &[("x-oauth-scopes", "repo, read:org"), ("x-accepted-oauth-scopes", "repo")],
        ));
    }

    // GET /api/v3/meta
    if rest.len() == 1 && rest[0] == "meta" {
        if method != "GET" {
            return Some(c.err_json(404, "Not Found"));
        }
        return Some(c.json_bytes(200, b"{\"installed_version\":\"1.0.0\"}".to_vec(), &[]));
    }

    // GET /api/v3/user
    if rest.len() == 1 && rest[0] == "user" {
        if method != "GET" {
            return Some(c.err_json(404, "Not Found"));
        }
        return Some(c.json_bytes(200, user_json(), &[]));
    }

    // /api/v3/user/repos
    if rest.len() == 2 && rest[0] == "user" && rest[1] == "repos" {
        if method == "POST" {
            let parsed = match c.parse_body() {
                Some(p) => p,
                None => return Some(c.err_json(400, "Problems parsing JSON")),
            };
            let name = body_str(&parsed, "name");
            if name.is_empty() || !name_valid(&name) || name.ends_with(".git") {
                return Some(c.err_json(422, "name is required and must be a valid repository name"));
            }
            if c.store.get_repo(VIEWER_LOGIN, &name).is_some() {
                return Some(c.err_json(422, "name already exists on this account"));
            }
            let repo = c.store.create_repo(
                VIEWER_LOGIN,
                &name,
                body_b(&parsed, "private"),
                &body_str(&parsed, "description"),
            );
            return match repo {
                Some(r) => Some(c.json_bytes(201, repo_json(&r), &[])),
                None => Some(c.err_json(422, "name already exists on this account")),
            };
        }
        if method == "GET" {
            let all = c.store.sorted_repos();
            let filtered: Vec<&Arc<RepoState>> =
                all.iter().filter(|r| r.owner == VIEWER_LOGIN).collect();
            let (start, end, link) = c.paginate(filtered.len(), "/api/v3/user/repos");
            let mut extra: Vec<(&str, &str)> = Vec::new();
            let mut link_owned = String::new();
            if !link.is_empty() {
                link_owned = link;
                extra.push(("link", &link_owned));
            }
            let mut w: Vec<u8> = Vec::new();
            w.push(b'[');
            for (i, r) in filtered[start..end].iter().enumerate() {
                if i > 0 {
                    w.push(b',');
                }
                w.extend_from_slice(&repo_json(r));
            }
            w.push(b']');
            return Some(c.json_bytes(200, w, &extra));
        }
        return Some(c.err_json(404, "Not Found"));
    }

    // /api/v3/repos/{owner}/{repo}[...]
    if rest.len() >= 3 && rest[0] == "repos" {
        let owner = must_decode(&rest[1]);
        let name = must_decode(&rest[2]);
        let tail: Vec<String> = rest[3..].to_vec();
        let repo = match c.repo_or_404(&owner, &name) {
            Some(r) => r,
            None => return Some(c.err_json(404, "Not Found")),
        };
        return Some(handle_repo_rest(c, &repo, &tail));
    }

    Some(c.err_json(404, "Not Found"))
}

fn handle_repo_rest(c: &Ctx, repo: &Arc<RepoState>, tail: &[String]) -> HttpResponse {
    let method = c.method;
    let store = c.store;

    // /repos/{o}/{r} itself
    if tail.is_empty() {
        if method == "GET" {
            return c.json_bytes(200, repo_json(repo), &[]);
        }
        if method == "PATCH" {
            let parsed = match c.parse_body() {
                Some(p) => p,
                None => return c.err_json(400, "Problems parsing JSON"),
            };
            let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                let mut nr = r.fork();
                if let Some(Json::Str(s)) = parsed.iter().find(|(k, _)| k == "description").map(|(_, v)| v) {
                    nr.description = s.clone();
                }
                if let Some(Json::Bool(b)) = parsed.iter().find(|(k, _)| k == "private").map(|(_, v)| v) {
                    nr.is_private = *b;
                }
                if let Some(Json::Str(s)) = parsed.iter().find(|(k, _)| k == "default_branch").map(|(_, v)| v) {
                    if name_valid(s) {
                        nr.default_branch = s.clone();
                    }
                }
                nr.updated_at = iso_of(store.next_ts());
                (Some(Arc::new(nr)), ())
            });
            return match out {
                Some(()) => match store.get_repo(&repo.owner, &repo.name) {
                    Some(r) => c.json_bytes(200, repo_json(&r), &[]),
                    None => c.err_json(404, "Not Found"),
                },
                None => c.err_json(404, "Not Found"),
            };
        }
        if method == "DELETE" {
            store.delete_repo(&repo.owner, &repo.name);
            let mut resp = HttpResponse::new(
                204,
                ResponseBody::Owned(Vec::new()),
            );
            resp.headers = vec![("x-github-request-id".to_string(), store.next_req_id())];
            resp.no_content_type = true;
            resp.no_content_length = true;
            return resp;
        }
        return c.err_json(404, "Not Found");
    }

    let head = tail[0].as_str();

    // /branches
    if head == "branches" && tail.len() == 1 {
        if method != "GET" {
            return c.err_json(404, "Not Found");
        }
        let mut branches: Vec<(String, String)> = repo
            .refs
            .map
            .iter()
            .filter(|(k, _)| k.starts_with("refs/heads/"))
            .map(|(k, v)| (k["refs/heads/".len()..].to_string(), v.clone()))
            .collect();
        branches.sort_by(|a, b| a.0.cmp(&b.0));
        let mut w: Vec<u8> = Vec::new();
        w.push(b'[');
        for (i, (name, sha)) in branches.iter().enumerate() {
            if i > 0 {
                w.push(b',');
            }
            w.extend_from_slice(b"{\"name\":");
            crate::json::push_json_string(&mut w, name);
            w.extend_from_slice(b",\"commit\":{\"sha\":");
            crate::json::push_json_string(&mut w, sha);
            w.extend_from_slice(b",\"url\":");
            crate::json::push_json_string(
                &mut w,
                &format!("/api/v3/repos/{}/{}/commits/{}", repo.owner, repo.name, sha),
            );
            w.extend_from_slice(b"},\"protected\":false}");
        }
        w.push(b']');
        return c.json_bytes(200, w, &[]);
    }

    // /commits
    if head == "commits" && tail.len() == 1 {
        if method != "GET" {
            return c.err_json(404, "Not Found");
        }
        let sha_param = query_get(c.query, "sha");
        let lookup = Store::lookup_fn(store);
        let (tip, tip_ok) = if !sha_param.is_empty() {
            if is_oid(sha_param) {
                (sha_param.to_string(), true)
            } else if let Some(v) = repo.refs.map.get(&format!("refs/heads/{}", sha_param)) {
                (v.clone(), true)
            } else {
                (String::new(), false)
            }
        } else if let Some(v) = repo.refs.map.get(&format!("refs/heads/{}", repo.default_branch)) {
            (v.clone(), true)
        } else {
            (String::new(), false)
        };
        if !tip_ok || store.lookup(&tip).is_none() {
            return c.json_bytes(200, b"[]".to_vec(), &[]);
        }
        let mut w: Vec<u8> = Vec::new();
        w.push(b'[');
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        queue.push_back(tip);
        let mut first = true;
        while let Some(oid) = queue.pop_front() {
            if !seen.insert(oid.clone()) {
                continue;
            }
            let obj = match store.lookup(&oid) {
                Some(o) if o.typ == ObjType::Commit => o,
                _ => continue,
            };
            let commit = parse_commit(&obj.data);
            if !first {
                w.push(b',');
            }
            first = false;
            w.extend_from_slice(b"{\"sha\":");
            crate::json::push_json_string(&mut w, &oid);
            w.extend_from_slice(b",\"commit\":{\"message\":");
            crate::json::push_json_string(&mut w, &commit.message);
            w.extend_from_slice(b",\"author\":{\"name\":");
            crate::json::push_json_string(&mut w, VIEWER_NAME);
            w.extend_from_slice(b",\"email\":");
            crate::json::push_json_string(&mut w, VIEWER_EMAIL);
            w.extend_from_slice(b",\"date\":");
            crate::json::push_json_string(&mut w, &iso_of(crate::store::BASE_TS));
            w.extend_from_slice(b"},\"committer\":{\"name\":");
            crate::json::push_json_string(&mut w, VIEWER_NAME);
            w.extend_from_slice(b",\"email\":");
            crate::json::push_json_string(&mut w, VIEWER_EMAIL);
            w.extend_from_slice(b",\"date\":");
            crate::json::push_json_string(&mut w, &iso_of(crate::store::BASE_TS));
            w.extend_from_slice(b"},\"tree\":{\"sha\":");
            crate::json::push_json_string(&mut w, &commit.tree);
            w.extend_from_slice(b"}},\"url\":");
            crate::json::push_json_string(
                &mut w,
                &format!("/api/v3/repos/{}/{}/commits/{}", repo.owner, repo.name, oid),
            );
            w.extend_from_slice(b"}");
            for p in &commit.parents {
                queue.push_back(p.clone());
            }
        }
        w.push(b']');
        let _ = lookup;
        return c.json_bytes(200, w, &[]);
    }

    // /git/refs
    if head == "git" && tail.len() > 1 && tail[1] == "refs" {
        if method == "POST" && tail.len() == 2 {
            let parsed = match c.parse_body() {
                Some(p) => p,
                None => return c.err_json(400, "Problems parsing JSON"),
            };
            let ref_name = body_str(&parsed, "ref");
            let sha = body_str(&parsed, "sha");
            if !ref_name.starts_with("refs/") || !is_oid(&sha) || store.lookup(&sha).is_none() {
                return c.err_json(422, "Invalid ref or sha");
            }
            if repo.refs.map.contains_key(&ref_name) {
                return c.err_json(422, "Reference already exists");
            }
            let ref_name2 = ref_name.clone();
            let sha2 = sha.clone();
            let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                let mut refs = (*r.refs).clone();
                refs.set(&ref_name2, &sha2);
                let mut nr = r.fork();
                nr.refs = Arc::new(refs);
                nr.gen += 1;
                (Some(Arc::new(nr)), ())
            });
            if out.is_none() {
                return c.err_json(404, "Not Found");
            }
            let mut w: Vec<u8> = Vec::new();
            w.extend_from_slice(b"{\"ref\":");
            crate::json::push_json_string(&mut w, &ref_name);
            w.extend_from_slice(b",\"object\":{\"sha\":");
            crate::json::push_json_string(&mut w, &sha);
            w.extend_from_slice(b",\"type\":\"commit\"},\"url\":");
            crate::json::push_json_string(
                &mut w,
                &format!("/api/v3/repos/{}/{}/git/refs/{}", repo.owner, repo.name, ref_name),
            );
            w.extend_from_slice(b"}");
            return c.json_bytes(201, w, &[]);
        }
        if method == "GET" {
            if tail.len() == 2 {
                let mut w: Vec<u8> = Vec::new();
                w.push(b'[');
                for (i, ref_name) in repo.refs.order.iter().enumerate() {
                    if i > 0 {
                        w.push(b',');
                    }
                    let sha = &repo.refs.map[ref_name];
                    w.extend_from_slice(b"{\"ref\":");
                    crate::json::push_json_string(&mut w, ref_name);
                    w.extend_from_slice(b",\"object\":{\"sha\":");
                    crate::json::push_json_string(&mut w, sha);
                    w.extend_from_slice(b",\"type\":\"commit\"},\"url\":");
                    crate::json::push_json_string(
                        &mut w,
                        &format!("/api/v3/repos/{}/{}/git/refs/{}", repo.owner, repo.name, ref_name),
                    );
                    w.extend_from_slice(b"}");
                }
                w.push(b']');
                return c.json_bytes(200, w, &[]);
            }
            let ref_name = format!("refs/{}", tail[2..].iter().map(|s| must_decode(s)).collect::<Vec<_>>().join("/"));
            let sha = match repo.refs.map.get(&ref_name) {
                Some(s) => s.clone(),
                None => return c.err_json(404, "Not Found"),
            };
            let mut w: Vec<u8> = Vec::new();
            w.extend_from_slice(b"{\"ref\":");
            crate::json::push_json_string(&mut w, &ref_name);
            w.extend_from_slice(b",\"object\":{\"sha\":");
            crate::json::push_json_string(&mut w, &sha);
            w.extend_from_slice(b",\"type\":\"commit\"},\"url\":");
            crate::json::push_json_string(
                &mut w,
                &format!("/api/v3/repos/{}/{}/git/refs/{}", repo.owner, repo.name, ref_name),
            );
            w.extend_from_slice(b"}");
            return c.json_bytes(200, w, &[]);
        }
        return c.err_json(404, "Not Found");
    }

    // /contents/{path...}
    if head == "contents" {
        let segments: Vec<String> = tail[1..].iter().map(|s| must_decode(s)).collect();
        return handle_contents(c, repo, &segments);
    }

    // /labels
    if head == "labels" {
        if method == "POST" && tail.len() == 1 {
            let parsed = match c.parse_body() {
                Some(p) => p,
                None => return c.err_json(400, "Problems parsing JSON"),
            };
            let name = body_str(&parsed, "name");
            let mut color = body_str(&parsed, "color");
            if color.is_empty() {
                color = "000000".to_string();
            }
            if name.is_empty() {
                return c.err_json(422, "name is required");
            }
            if repo.labels.contains_key(&name) {
                return c.err_json(422, "already_exists");
            }
            let name2 = name.clone();
            let color2 = color.clone();
            let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                let mut nr = r.fork();
                nr.label_counter += 1;
                let l = Label { id: nr.label_counter, name: name2.clone(), color: color2.clone() };
                let mut labels: HashMap<String, Arc<Label>> = (*r.labels).clone();
                labels.insert(name2.clone(), Arc::new(l.clone()));
                nr.labels = Arc::new(labels);
                (Some(Arc::new(nr)), l)
            });
            return match out {
                Some(l) => c.json_bytes(201, label_json(&l), &[]),
                None => c.err_json(404, "Not Found"),
            };
        }
        if method == "GET" && tail.len() == 1 {
            let mut labels: Vec<&Arc<Label>> = repo.labels.values().collect();
            labels.sort_by_key(|l| l.id);
            let mut w: Vec<u8> = Vec::new();
            w.push(b'[');
            for (i, l) in labels.iter().enumerate() {
                if i > 0 {
                    w.push(b',');
                }
                w.extend_from_slice(&label_json(l));
            }
            w.push(b']');
            return c.json_bytes(200, w, &[]);
        }
        return c.err_json(404, "Not Found");
    }

    // /issues
    if head == "issues" {
        if tail.len() == 1 {
            if method == "GET" {
                let mut state = query_get(c.query, "state");
                if state.is_empty() {
                    state = "open";
                }
                let mut list = sorted_issues(repo);
                if state != "all" {
                    list.retain(|iss| iss.state == state);
                }
                list = sort_issues_list(
                    list,
                    query_get(c.query, "sort"),
                    query_get(c.query, "direction"),
                );
                let (start, end, link) =
                    c.paginate(list.len(), &format!("/api/v3/repos/{}/{}/issues", repo.owner, repo.name));
                let mut extra: Vec<(&str, &str)> = Vec::new();
                let mut link_owned = String::new();
                if !link.is_empty() {
                    link_owned = link;
                    extra.push(("link", &link_owned));
                }
                let mut w: Vec<u8> = Vec::new();
                w.push(b'[');
                for (i, iss) in list[start..end].iter().enumerate() {
                    if i > 0 {
                        w.push(b',');
                    }
                    w.extend_from_slice(&iss.cached);
                }
                w.push(b']');
                return c.json_bytes(200, w, &extra);
            }
            if method == "POST" {
                let parsed = match c.parse_body() {
                    Some(p) => p,
                    None => return c.err_json(400, "Problems parsing JSON"),
                };
                let title = body_str(&parsed, "title");
                if title.is_empty() {
                    return c.err_json(422, "title is required");
                }
                let labels = body_list(&parsed, "labels").unwrap_or_default();
                for l in &labels {
                    if !repo.labels.contains_key(l) {
                        return c.err_json(422, &format!("label '{}' does not exist", l));
                    }
                }
                let title2 = title.clone();
                let body2 = body_str(&parsed, "body");
                let labels2 = labels.clone();
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let number = r.issue_counter + 1;
                    let ts = store.next_ts();
                    let iss = new_issue(r, number, &title2, &body2, labels2.clone(), ts);
                    let mut issues: HashMap<i64, Arc<Issue>> = (*r.issues).clone();
                    issues.insert(number, Arc::new(iss.clone()));
                    let mut nr = r.fork();
                    nr.issues = Arc::new(issues);
                    nr.issue_counter = number;
                    (Some(Arc::new(nr)), iss)
                });
                return match out {
                    Some(iss) => c.json_bytes(201, iss.cached.clone(), &[]),
                    None => c.err_json(404, "Not Found"),
                };
            }
            return c.err_json(404, "Not Found");
        }
        let num = js_number(&tail[1]).and_then(to_int_number);
        let iss = num.and_then(|n| repo.issues.get(&n)).cloned();
        let iss = match iss {
            Some(i) => i,
            None => return c.err_json(404, "Not Found"),
        };
        if tail.len() == 2 {
            if method == "GET" {
                return c.json_shared(200, Arc::new(iss.cached.clone()), &[]);
            }
            if method == "PATCH" {
                let parsed = match c.parse_body() {
                    Some(p) => p,
                    None => return c.err_json(400, "Problems parsing JSON"),
                };
                let number = iss.number;
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let cur = match r.issues.get(&number) {
                        Some(i) => (**i).clone(),
                        None => return (None, None),
                    };
                    let mut iss = cur;
                    if let Some(Json::Str(s)) = parsed.iter().find(|(k, _)| k == "state").map(|(_, v)| v) {
                        if s == "open" || s == "closed" {
                            iss.state = s.clone();
                            if s == "closed" {
                                iss.closed_at = iso_of(store.next_ts());
                                iss.has_closed = true;
                            } else {
                                iss.closed_at = String::new();
                                iss.has_closed = false;
                            }
                        }
                    }
                    if let Some(Json::Str(s)) = parsed.iter().find(|(k, _)| k == "title").map(|(_, v)| v) {
                        iss.title = s.clone();
                    }
                    if let Some(Json::Str(s)) = parsed.iter().find(|(k, _)| k == "body").map(|(_, v)| v) {
                        iss.body = s.clone();
                    }
                    if let Some(Json::Arr(arr)) = parsed.iter().find(|(k, _)| k == "labels").map(|(_, v)| v) {
                        let mut labels: Vec<String> = Vec::new();
                        for item in arr {
                            if let Json::Str(s) = item {
                                labels.push(s.clone());
                            }
                        }
                        for l in &labels {
                            if !r.labels.contains_key(l) {
                                return (None, Some(format!("label '{}' does not exist", l)));
                            }
                        }
                        iss.labels = labels;
                    }
                    iss.updated_at = iso_of(store.next_ts());
                    refresh_issue(r, &mut iss);
                    let mut issues: HashMap<i64, Arc<Issue>> = (*r.issues).clone();
                    issues.insert(number, Arc::new(iss.clone()));
                    let mut nr = r.fork();
                    nr.issues = Arc::new(issues);
                    (Some(Arc::new(nr)), None)
                });
                return match out {
                    Some(None) => match store.get_repo(&repo.owner, &repo.name) {
                        Some(r) => match r.issues.get(&number) {
                            Some(iss) => c.json_bytes(200, iss.cached.clone(), &[]),
                            None => c.err_json(404, "Not Found"),
                        },
                        None => c.err_json(404, "Not Found"),
                    },
                    Some(Some(msg)) => c.err_json(422, &msg),
                    None => c.err_json(404, "Not Found"),
                };
            }
            return c.err_json(404, "Not Found");
        }
        if tail.len() == 3 && tail[2] == "comments" {
            if method == "GET" {
                let mut w: Vec<u8> = Vec::new();
                w.push(b'[');
                for (i, cm) in iss.comments.iter().enumerate() {
                    if i > 0 {
                        w.push(b',');
                    }
                    w.extend_from_slice(&comment_json(repo, &iss, cm));
                }
                w.push(b']');
                return c.json_bytes(200, w, &[]);
            }
            if method == "POST" {
                let parsed = match c.parse_body() {
                    Some(p) => p,
                    None => return c.err_json(400, "Problems parsing JSON"),
                };
                let text = body_str(&parsed, "body");
                if text.is_empty() {
                    return c.err_json(422, "body is required");
                }
                let number = iss.number;
                let text2 = text.clone();
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let cur = match r.issues.get(&number) {
                        Some(i) => (**i).clone(),
                        None => return (None, None),
                    };
                    let mut iss = cur;
                    let ts = store.next_ts();
                    let cm = Arc::new(crate::store::Comment {
                        id: store.next_comment_id(),
                        body: text2.clone(),
                        created_at: iso_of(ts),
                        updated_at: iso_of(ts),
                    });
                    iss.comments.push(cm.clone());
                    refresh_issue(r, &mut iss);
                    let mut issues: HashMap<i64, Arc<Issue>> = (*r.issues).clone();
                    issues.insert(number, Arc::new(iss));
                    let mut nr = r.fork();
                    nr.issues = Arc::new(issues);
                    (Some(Arc::new(nr)), Some(cm))
                });
                return match out {
                    Some(Some(cm)) => match store.get_repo(&repo.owner, &repo.name) {
                        Some(r) => match r.issues.get(&number) {
                            Some(iss) => c.json_bytes(201, comment_json(&r, iss, &cm), &[]),
                            None => c.err_json(404, "Not Found"),
                        },
                        None => c.err_json(404, "Not Found"),
                    },
                    _ => c.err_json(404, "Not Found"),
                };
            }
            return c.err_json(404, "Not Found");
        }
        return c.err_json(404, "Not Found");
    }

    // /pulls
    if head == "pulls" {
        if tail.len() == 1 {
            if method == "GET" {
                let mut state = query_get(c.query, "state");
                if state.is_empty() {
                    state = "open";
                }
                let mut list = sorted_pulls(repo);
                if state != "all" {
                    list.retain(|pr| pr.state == state);
                }
                let (start, end, link) =
                    c.paginate(list.len(), &format!("/api/v3/repos/{}/{}/pulls", repo.owner, repo.name));
                let mut extra: Vec<(&str, &str)> = Vec::new();
                let mut link_owned = String::new();
                if !link.is_empty() {
                    link_owned = link;
                    extra.push(("link", &link_owned));
                }
                let mut w: Vec<u8> = Vec::new();
                w.push(b'[');
                for (i, pr) in list[start..end].iter().enumerate() {
                    if i > 0 {
                        w.push(b',');
                    }
                    w.extend_from_slice(&pr_json(store, repo, pr));
                }
                w.push(b']');
                return c.json_bytes(200, w, &extra);
            }
            if method == "POST" {
                let parsed = match c.parse_body() {
                    Some(p) => p,
                    None => return c.err_json(400, "Problems parsing JSON"),
                };
                let head_ref = body_str(&parsed, "head");
                let base_ref = body_str(&parsed, "base");
                let title = body_str(&parsed, "title");
                if head_ref.is_empty() || base_ref.is_empty() || title.is_empty() {
                    return c.err_json(422, "head, base and title are required");
                }
                if !repo.refs.map.contains_key(&format!("refs/heads/{}", head_ref))
                    || !repo.refs.map.contains_key(&format!("refs/heads/{}", base_ref))
                {
                    return c.err_json(422, "head or base branch does not exist");
                }
                let head2 = head_ref.clone();
                let base2 = base_ref.clone();
                let title2 = title.clone();
                let body2 = body_str(&parsed, "body");
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let head_sha = r.refs.map.get(&format!("refs/heads/{}", head2)).cloned();
                    let base_sha = r.refs.map.get(&format!("refs/heads/{}", base2)).cloned();
                    let (head_sha, base_sha) = match (head_sha, base_sha) {
                        (Some(h), Some(b)) => (h, b),
                        _ => return (None, None),
                    };
                    let number = r.pull_counter + 1;
                    let ts = store.next_ts();
                    let pr = PullRequest {
                        number,
                        id: number,
                        title: title2.clone(),
                        body: body2.clone(),
                        state: "open".to_string(),
                        head_ref: head2.clone(),
                        base_ref: base2.clone(),
                        head_sha,
                        base_sha,
                        merged: false,
                        merged_at: String::new(),
                        merge_commit: String::new(),
                        created_at: iso_of(ts),
                        updated_at: iso_of(ts),
                        closed_at: String::new(),
                        has_closed: false,
                    };
                    let mut pulls: HashMap<i64, Arc<PullRequest>> = (*r.pulls).clone();
                    pulls.insert(number, Arc::new(pr.clone()));
                    let mut nr = r.fork();
                    nr.pulls = Arc::new(pulls);
                    nr.pull_counter = number;
                    (Some(Arc::new(nr)), Some(pr))
                });
                return match out {
                    Some(Some(pr)) => match store.get_repo(&repo.owner, &repo.name) {
                        Some(r) => c.json_bytes(201, pr_json(store, &r, &pr), &[]),
                        None => c.err_json(404, "Not Found"),
                    },
                    _ => c.err_json(422, "could not create pull request"),
                };
            }
            return c.err_json(404, "Not Found");
        }
        let num = js_number(&tail[1]).and_then(to_int_number);
        let pr = num.and_then(|n| repo.pulls.get(&n)).cloned();
        let pr = match pr {
            Some(p) => p,
            None => return c.err_json(404, "Not Found"),
        };
        if tail.len() == 2 {
            if method == "GET" {
                return c.json_bytes(200, pr_json(store, repo, &pr), &[]);
            }
            return c.err_json(404, "Not Found");
        }
        if tail.len() == 3 && tail[2] == "files" {
            if method != "GET" {
                return c.err_json(404, "Not Found");
            }
            let base_tip = repo
                .refs
                .map
                .get(&format!("refs/heads/{}", pr.base_ref))
                .cloned()
                .unwrap_or_else(|| pr.base_sha.clone());
            let head_tip = repo
                .refs
                .map
                .get(&format!("refs/heads/{}", pr.head_ref))
                .cloned()
                .unwrap_or_else(|| pr.head_sha.clone());
            let lookup = Store::lookup_fn(store);
            let files = diff_trees(&base_tip, &head_tip, &lookup);
            let mut w: Vec<u8> = Vec::new();
            w.push(b'[');
            for (i, f) in files.iter().enumerate() {
                if i > 0 {
                    w.push(b',');
                }
                w.extend_from_slice(b"{\"filename\":");
                crate::json::push_json_string(&mut w, &f.filename);
                w.extend_from_slice(b",\"status\":");
                crate::json::push_json_string(&mut w, &f.status);
                w.extend_from_slice(b",\"additions\":");
                w.extend_from_slice(f.additions.to_string().as_bytes());
                w.extend_from_slice(b",\"deletions\":");
                w.extend_from_slice(f.deletions.to_string().as_bytes());
                w.extend_from_slice(b",\"changes\":");
                w.extend_from_slice(f.changes.to_string().as_bytes());
                w.extend_from_slice(b"}");
            }
            w.push(b']');
            return c.json_bytes(200, w, &[]);
        }
        if tail.len() == 3 && tail[2] == "merge" {
            if method != "PUT" {
                return c.err_json(404, "Not Found");
            }
            if c.parse_body().is_none() {
                return c.err_json(400, "Problems parsing JSON");
            }
            let number = pr.number;
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
            return match out {
                Some(Ok(sha)) => {
                    let mut w: Vec<u8> = Vec::new();
                    w.extend_from_slice(b"{\"merged\":true,\"sha\":");
                    crate::json::push_json_string(&mut w, &sha);
                    w.extend_from_slice(b",\"message\":\"Pull request successfully merged\"}");
                    c.json_bytes(200, w, &[])
                }
                Some(Err("not-found")) => c.err_json(404, "Not Found"),
                Some(Err("already")) => c.err_json(405, "Pull request is already merged"),
                Some(Err("conflict")) => c.err_json(405, "Merge conflict"),
                Some(Err(_)) => c.err_json(404, "Not Found"),
                None => c.err_json(404, "Not Found"),
            };
        }
        return c.err_json(404, "Not Found");
    }

    c.err_json(404, "Not Found")
}

fn handle_contents(c: &Ctx, repo: &Arc<RepoState>, segments: &[String]) -> HttpResponse {
    let method = c.method;
    let store = c.store;
    let lookup = Store::lookup_fn(store);

    let resolve_ref = || -> Option<String> {
        let ref_param = query_get(c.query, "ref");
        if !ref_param.is_empty() {
            if is_oid(ref_param) {
                if store.lookup(ref_param).is_some() {
                    return Some(ref_param.to_string());
                }
                return None;
            }
            if let Some(v) = repo.refs.map.get(&format!("refs/heads/{}", ref_param)) {
                return Some(v.clone());
            }
            return None;
        }
        repo.refs.map.get(&format!("refs/heads/{}", repo.default_branch)).cloned()
    };

    if method == "GET" {
        let tip = match resolve_ref() {
            Some(t) => t,
            None => return c.err_json(404, "Not Found"),
        };
        let commit_obj = match store.lookup(&tip) {
            Some(o) if o.typ == ObjType::Commit => o,
            _ => return c.err_json(404, "Not Found"),
        };
        let root_tree = parse_commit(&commit_obj.data).tree;
        if segments.is_empty() {
            let entries = list_dir(&root_tree, &[], &lookup).unwrap_or_default();
            return c.json_bytes(200, dir_listing(c, repo, &entries, ""), &[]);
        }
        if let Some(entries) = list_dir(&root_tree, segments, &lookup) {
            let prefix = segments.join("/");
            return c.json_bytes(200, dir_listing(c, repo, &entries, &prefix), &[]);
        }
        let blob_oid = get_path(&root_tree, segments, &lookup);
        if blob_oid.is_empty() {
            return c.err_json(404, "Not Found");
        }
        let blob = match store.lookup(&blob_oid) {
            Some(b) => b,
            None => return c.err_json(404, "Not Found"),
        };
        let mut w: Vec<u8> = Vec::new();
        w.extend_from_slice(b"{\"type\":\"file\",\"encoding\":\"base64\",\"content\":");
        crate::json::push_json_string(&mut w, &b64encode(&blob.data));
        w.extend_from_slice(b",\"sha\":");
        crate::json::push_json_string(&mut w, &blob_oid);
        w.extend_from_slice(b",\"size\":");
        w.extend_from_slice(blob.data.len().to_string().as_bytes());
        w.extend_from_slice(b",\"name\":");
        crate::json::push_json_string(&mut w, &segments[segments.len() - 1]);
        w.extend_from_slice(b",\"path\":");
        crate::json::push_json_string(&mut w, &segments.join("/"));
        w.extend_from_slice(b"}");
        return c.json_bytes(200, w, &[]);
    }

    if method == "PUT" || method == "DELETE" {
        let parsed = match c.parse_body() {
            Some(p) => p,
            None => return c.err_json(400, "Problems parsing JSON"),
        };
        let mut branch = body_str(&parsed, "branch");
        if branch.is_empty() {
            branch = repo.default_branch.clone();
        }
        let tip = repo.refs.map.get(&format!("refs/heads/{}", branch)).cloned();
        let has_tip = tip.is_some();
        let mut root_tree = String::new();
        if let Some(t) = &tip {
            if let Some(obj) = store.lookup(t) {
                root_tree = parse_commit(&obj.data).tree;
            }
        }

        if method == "PUT" {
            let mut message = body_str(&parsed, "message");
            if message.is_empty() {
                message = "Update file".to_string();
            }
            let content = body_str(&parsed, "content");
            if content.is_empty() {
                return c.err_json(422, "content is required");
            }
            let bytes = b64decode(&content);
            let existing = get_path(&root_tree, segments, &lookup);
            let sha = body_str(&parsed, "sha");
            if !existing.is_empty() {
                if sha.is_empty() {
                    return c.err_json(422, "sha is required to update a file");
                }
                if sha != existing {
                    return c.err_json(409, "sha does not match the current file");
                }
            }
            let blob_oid = store.put_object(ObjType::Blob, bytes.clone());
            let branch2 = branch.clone();
            let segments2: Vec<String> = segments.to_vec();
            let message2 = message.clone();
            let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                // Re-resolve the tip inside the lock (it may have moved).
                let tip = r.refs.map.get(&format!("refs/heads/{}", branch2)).cloned();
                let has_tip = tip.is_some();
                let mut root_tree = String::new();
                if let Some(t) = &tip {
                    if let Some(obj) = store.lookup(t) {
                        root_tree = parse_commit(&obj.data).tree;
                    }
                }
                let lookup = Store::lookup_fn(store);
                let mut put = |t: ObjType, data: Vec<u8>| store.put_object(t, data);
                let new_tree = set_path(&root_tree, &segments2, &blob_oid, &lookup, &mut put);
                let parents: Vec<String> = if has_tip { vec![tip.unwrap()] } else { vec![] };
                let (nr, commit_oid) = store.commit_on(r, &branch2, &new_tree, &parents, &message2);
                (Some(nr), commit_oid)
            });
            let commit_oid = match out {
                Some(oid) => oid,
                None => return c.err_json(404, "Not Found"),
            };
            let status = if existing.is_empty() { 201 } else { 200 };
            let mut w: Vec<u8> = Vec::new();
            w.extend_from_slice(b"{\"content\":{\"name\":");
            crate::json::push_json_string(&mut w, &segments[segments.len() - 1]);
            w.extend_from_slice(b",\"path\":");
            crate::json::push_json_string(&mut w, &segments.join("/"));
            w.extend_from_slice(b",\"sha\":");
            crate::json::push_json_string(&mut w, &blob_oid);
            w.extend_from_slice(b",\"size\":");
            w.extend_from_slice(bytes.len().to_string().as_bytes());
            w.extend_from_slice(b",\"encoding\":\"base64\"},\"commit\":{\"sha\":");
            crate::json::push_json_string(&mut w, &commit_oid);
            w.extend_from_slice(b",\"message\":");
            crate::json::push_json_string(&mut w, &message);
            w.extend_from_slice(b"}}");
            return c.json_bytes(status, w, &[]);
        }

        // DELETE
        let mut message = body_str(&parsed, "message");
        if message.is_empty() {
            message = "Delete file".to_string();
        }
        let sha = body_str(&parsed, "sha");
        let existing = get_path(&root_tree, segments, &lookup);
        if existing.is_empty() {
            return c.err_json(404, "Not Found");
        }
        if sha.is_empty() || sha != existing {
            return c.err_json(409, "sha does not match the current file");
        }
        let branch2 = branch.clone();
        let segments2: Vec<String> = segments.to_vec();
        let message2 = message.clone();
        let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
            let tip = r.refs.map.get(&format!("refs/heads/{}", branch2)).cloned();
            let has_tip = tip.is_some();
            let mut root_tree = String::new();
            if let Some(t) = &tip {
                if let Some(obj) = store.lookup(t) {
                    root_tree = parse_commit(&obj.data).tree;
                }
            }
            let lookup = Store::lookup_fn(store);
            let mut put = |t: ObjType, data: Vec<u8>| store.put_object(t, data);
            let new_tree = delete_path(&root_tree, &segments2, &lookup, &mut put);
            let parents: Vec<String> = if has_tip { vec![tip.unwrap()] } else { vec![] };
            let (nr, commit_oid) = store.commit_on(r, &branch2, &new_tree, &parents, &message2);
            (Some(nr), commit_oid)
        });
        let commit_oid = match out {
            Some(oid) => oid,
            None => return c.err_json(404, "Not Found"),
        };
        let mut w: Vec<u8> = Vec::new();
        w.extend_from_slice(b"{\"commit\":{\"sha\":");
        crate::json::push_json_string(&mut w, &commit_oid);
        w.extend_from_slice(b",\"message\":");
        crate::json::push_json_string(&mut w, &message);
        w.extend_from_slice(b"}}");
        return c.json_bytes(200, w, &[]);
    }

    c.err_json(404, "Not Found")
}

fn dir_listing(c: &Ctx, repo: &RepoState, entries: &[crate::gitobj::TreeEntry], prefix: &str) -> Vec<u8> {
    let mut w: Vec<u8> = Vec::new();
    w.push(b'[');
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            w.push(b',');
        }
        let mut size = 0usize;
        if e.mode != "40000" {
            if let Some(obj) = c.store.lookup(&e.oid) {
                size = obj.data.len();
            }
        }
        w.extend_from_slice(b"{\"type\":");
        if e.mode == "40000" {
            crate::json::push_json_string(&mut w, "dir");
        } else {
            crate::json::push_json_string(&mut w, "file");
        }
        w.extend_from_slice(b",\"name\":");
        crate::json::push_json_string(&mut w, &e.name);
        w.extend_from_slice(b",\"path\":");
        if prefix.is_empty() {
            crate::json::push_json_string(&mut w, &e.name);
        } else {
            crate::json::push_json_string(&mut w, &format!("{}/{}", prefix, e.name));
        }
        w.extend_from_slice(b",\"sha\":");
        crate::json::push_json_string(&mut w, &e.oid);
        w.extend_from_slice(b",\"size\":");
        w.extend_from_slice(size.to_string().as_bytes());
        w.extend_from_slice(b"}");
    }
    w.push(b']');
    w
}
