// Git smart HTTP (v2 + v0), mirroring the Go forge's behaviour and
// responses. Caches are layered on top: the v2 advertisement is constant
// bytes, the v0 advertisement and ls-refs responses are cached per repo
// snapshot, and fetch responses per snapshot plus the raw request body.
// The fetch cache mutex is held during computation so the clone workload's
// 50 concurrent identical fetches compute once and hit 49 times.

use crate::gitobj::{
    decode_pkt_lines, is_descendant, parse_commit, parse_tag, parse_tree, pkt_line, read_pack,
    write_pack, GitObject, ObjType, PktKind, ZERO_OID,
};
use crate::http::ResponseBody;
use crate::server::{must_decode, split_path, Ctx};
use crate::store::{RepoState, Store};
use crate::util::{is_oid_bytes, js_number};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub fn handle_git(c: &Ctx) -> Option<crate::http::HttpResponse> {
    let seg = split_path(c.path);
    if seg.len() < 2 || !seg[1].ends_with(".git") {
        return None;
    }
    let owner = must_decode(&seg[0]);
    let name = must_decode(&seg[1][..seg[1].len() - 4]);
    if !c.authorized() {
        return Some(c.err_json(401, "Requires authentication"));
    }
    let repo = c.repo_or_404(&owner, &name)?;
    if c.method == "GET" && seg.len() == 3 && seg[2] == "info" {
        return Some(c.err_json(404, "Not Found"));
    }
    if c.method == "GET" && seg.len() == 4 && seg[2] == "info" && seg[3] == "refs" {
        let service = crate::http::query_get(c.query, "service");
        let service = if service.is_empty() { "git-upload-pack" } else { service };
        return Some(handle_info_refs(c, &repo, service));
    }
    if c.method == "POST" && seg.len() == 3 && seg[2] == "git-upload-pack" {
        let is_v2 = c.git_protocol.starts_with("version=2");
        return Some(handle_upload_pack(c, &repo, is_v2));
    }
    if c.method == "POST" && seg.len() == 3 && seg[2] == "git-receive-pack" {
        return Some(handle_receive_pack(c, &repo));
    }
    Some(c.err_json(404, "Not Found"))
}

fn git_resp(c: &Ctx, content_type: &str, body: ResponseBody) -> crate::http::HttpResponse {
    let mut resp = c.json_resp(200, body, &[]);
    resp.content_type = Some(content_type.to_string());
    // git responses carry the request id but no content-type override beyond
    // the git one; json_resp already set the default — replace headers only.
    resp
}

fn git_resp_bytes(c: &Ctx, content_type: &str, body: Vec<u8>) -> crate::http::HttpResponse {
    git_resp(c, content_type, ResponseBody::Owned(body))
}

fn git_resp_shared(c: &Ctx, content_type: &str, body: Arc<Vec<u8>>) -> crate::http::HttpResponse {
    git_resp(c, content_type, ResponseBody::Shared(body))
}

fn v2_advertisement() -> Vec<u8> {
    let mut out = Vec::new();
    for line in ["version 2\n", "ls-refs=unborn\n", "fetch=shallow\n", "object-format=sha1\n"] {
        out.extend_from_slice(&pkt_line(line.as_bytes()));
    }
    out.extend_from_slice(b"0000");
    out
}

fn ref_entries(repo: &RepoState) -> Vec<(String, String)> {
    let mut names: Vec<String> = repo.refs.map.keys().cloned().collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let oid = repo.refs.map[&n].clone();
            (n, oid)
        })
        .collect()
}

fn peeled_of(store: &Store, oid: &str) -> String {
    match store.lookup(oid) {
        Some(obj) if obj.typ == ObjType::Tag => parse_tag(&obj.data).object,
        _ => String::new(),
    }
}

fn v0_advertisement(store: &Store, repo: &RepoState, service: &str) -> Arc<Vec<u8>> {
    let slot = if service == "git-upload-pack" { &repo.adv_up } else { &repo.adv_recv };
    let mut guard = slot.lock().unwrap();
    if let Some(cached) = guard.as_ref() {
        return cached.clone();
    }
    let caps = if service == "git-receive-pack" {
        "symref=HEAD:refs/heads/main report-status side-band-64k delete-refs"
    } else {
        "symref=HEAD:refs/heads/main side-band-64k shallow thin-pack ofs-delta multi_ack"
    };
    let mut parts: Vec<u8> = Vec::new();
    parts.extend_from_slice(&pkt_line(format!("# service={}\n", service).as_bytes()));
    parts.extend_from_slice(b"0000");
    let refs = ref_entries(repo);
    if refs.is_empty() {
        parts.extend_from_slice(&pkt_line(
            format!("{} capabilities^{{}}\x00{}\n", ZERO_OID, caps).as_bytes(),
        ));
    } else {
        if let Some(main_tip) = repo.refs.map.get(&format!("refs/heads/{}", repo.default_branch)) {
            parts.extend_from_slice(&pkt_line(
                format!("{} HEAD\x00{}\n", main_tip, caps).as_bytes(),
            ));
        } else {
            parts.extend_from_slice(&pkt_line(
                format!("{} {}\x00{}\n", refs[0].1, refs[0].0, caps).as_bytes(),
            ));
        }
        for (name, oid) in &refs {
            parts.extend_from_slice(&pkt_line(format!("{} {}\n", oid, name).as_bytes()));
            let peeled = peeled_of(store, oid);
            if !peeled.is_empty() {
                parts.extend_from_slice(&pkt_line(format!("{} {}^{{}}\n", peeled, name).as_bytes()));
            }
        }
    }
    parts.extend_from_slice(b"0000");
    let body = Arc::new(parts);
    *guard = Some(body.clone());
    body
}

fn handle_info_refs(c: &Ctx, repo: &RepoState, service: &str) -> crate::http::HttpResponse {
    let store = c.store;
    if service == "git-upload-pack" {
        if c.git_protocol.starts_with("version=2") {
            return git_resp_bytes(
                c,
                "application/x-git-upload-pack-advertisement",
                v2_advertisement(),
            );
        }
        let body = v0_advertisement(store, repo, service);
        return git_resp_shared(c, "application/x-git-upload-pack-advertisement", body);
    }
    if service == "git-receive-pack" {
        let body = v0_advertisement(store, repo, service);
        return git_resp_shared(c, "application/x-git-receive-pack-advertisement", body);
    }
    c.err_json(404, "Not Found")
}

// --- ls-refs ---

#[derive(Default, Clone)]
struct LsRefsArgs {
    peel: bool,
    symrefs: bool,
    unborn: bool,
    prefixes: Vec<String>,
}

fn parse_ls_refs_args(packets: &[crate::gitobj::Pkt], body: &[u8]) -> LsRefsArgs {
    let mut args = LsRefsArgs::default();
    let mut seen_delim = false;
    for p in packets {
        if p.kind == PktKind::Delim {
            seen_delim = true;
            continue;
        }
        if p.kind != crate::gitobj::PktKind::Data {
            continue;
        }
        if !seen_delim {
            continue;
        }
        let line = String::from_utf8_lossy(&body[p.start..p.end]).into_owned();
        match line.as_str() {
            "peel\n" => args.peel = true,
            "symrefs\n" => args.symrefs = true,
            "unborn\n" => args.unborn = true,
            _ => {
                if let Some(rest) = line.strip_prefix("ref-prefix ") {
                    args.prefixes.push(rest.strip_suffix('\n').unwrap_or(rest).to_string());
                }
            }
        }
    }
    args
}

fn ls_refs_key(args: &LsRefsArgs) -> String {
    let mut s = String::new();
    if args.peel {
        s.push_str("peel;");
    }
    if args.symrefs {
        s.push_str("symrefs;");
    }
    if args.unborn {
        s.push_str("unborn;");
    }
    for p in &args.prefixes {
        s.push_str("prefix=");
        s.push_str(p);
        s.push(';');
    }
    s
}

fn ls_refs_response(store: &Store, repo: &RepoState, args: &LsRefsArgs) -> Arc<Vec<u8>> {
    let key = ls_refs_key(args);
    let mut cache = repo.ls_cache.lock().unwrap();
    if let Some(cached) = cache.get(&key) {
        return cached.clone();
    }
    let keep = |name: &str| -> bool {
        if args.prefixes.is_empty() {
            return true;
        }
        args.prefixes.iter().any(|p| name.starts_with(p.as_str()))
    };
    let mut lines: Vec<String> = Vec::new();
    if let Some(main_tip) = repo.refs.map.get(&format!("refs/heads/{}", repo.default_branch)) {
        if keep("HEAD") {
            if args.symrefs {
                lines.push(format!("{} HEAD symref-target:refs/heads/{}\n", main_tip, repo.default_branch));
            } else {
                lines.push(format!("{} HEAD\n", main_tip));
            }
        }
    } else if args.unborn && keep("HEAD") {
        lines.push(format!("unborn HEAD symref-target:refs/heads/{}\n", repo.default_branch));
    }
    for (name, oid) in ref_entries(repo) {
        if !keep(&name) {
            continue;
        }
        let mut line = format!("{} {}", oid, name);
        if args.peel {
            let peeled = peeled_of(store, &oid);
            if !peeled.is_empty() {
                line.push_str(&format!(" peeled:{}", peeled));
            }
        }
        line.push('\n');
        lines.push(line);
    }
    let mut body: Vec<u8> = Vec::new();
    for l in &lines {
        body.extend_from_slice(&pkt_line(l.as_bytes()));
    }
    body.extend_from_slice(b"0000");
    let body = Arc::new(body);
    cache.insert(key, body.clone());
    body
}

// --- fetch ---

#[derive(Default)]
struct FetchArgs {
    wants: Vec<String>,
    haves: Vec<String>,
    shallow: Vec<String>,
    depth: i64,
    has_depth: bool,
}

fn parse_fetch_args(packets: &[crate::gitobj::Pkt], body: &[u8]) -> FetchArgs {
    let mut args = FetchArgs::default();
    let mut seen_delim = false;
    for p in packets {
        if p.kind == PktKind::Delim {
            seen_delim = true;
            continue;
        }
        if p.kind != crate::gitobj::PktKind::Data {
            continue;
        }
        if !seen_delim {
            continue;
        }
        let line = String::from_utf8_lossy(&body[p.start..p.end]).into_owned();
        if let Some(rest) = line.strip_prefix("want ") {
            args.wants.push(rest.strip_suffix('\n').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("have ") {
            args.haves.push(rest.strip_suffix('\n').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("shallow ") {
            args.shallow.push(rest.strip_suffix('\n').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("deepen ") {
            let num = rest.strip_suffix('\n').unwrap_or(rest);
            if let Some(f) = js_number(num) {
                if f == (f as i64) as f64 && f > 0.0 {
                    args.depth = f as i64;
                    args.has_depth = true;
                }
            }
        }
    }
    args
}

/// Every object reachable from roots without following commit parents.
fn shallow_closure(roots: &[String], lookup: &crate::gitobj::Lookup) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = Vec::new();
    for root in roots {
        if is_oid_bytes(root.as_bytes()) {
            queue.push(root.clone());
        }
    }
    while let Some(oid) = queue.pop() {
        if seen.contains(&oid) {
            continue;
        }
        let obj = match lookup(&oid) {
            Some(o) => o,
            None => continue,
        };
        seen.insert(oid);
        match obj.typ {
            ObjType::Commit => queue.push(parse_commit(&obj.data).tree),
            ObjType::Tag => queue.push(parse_tag(&obj.data).object),
            ObjType::Tree => {
                if let Ok(entries) = parse_tree(&obj.data) {
                    for e in entries {
                        if e.mode != "160000" {
                            queue.push(e.oid);
                        }
                    }
                }
            }
            ObjType::Blob => {}
        }
    }
    seen
}

/// The closure of the client's haves, stopping at shallow boundaries.
fn have_closure(roots: &[String], shallow: &[String], lookup: &crate::gitobj::Lookup) -> HashSet<String> {
    let shallow_set: HashSet<&str> = shallow.iter().map(|s| s.as_str()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = Vec::new();
    for root in roots {
        if is_oid_bytes(root.as_bytes()) {
            queue.push(root.clone());
        }
    }
    while let Some(oid) = queue.pop() {
        if seen.contains(&oid) {
            continue;
        }
        let obj = match lookup(&oid) {
            Some(o) => o,
            None => continue,
        };
        seen.insert(oid.clone());
        match obj.typ {
            ObjType::Commit => {
                let commit = parse_commit(&obj.data);
                queue.push(commit.tree);
                if !shallow_set.contains(oid.as_str()) {
                    queue.extend(commit.parents.iter().cloned());
                }
            }
            ObjType::Tag => queue.push(parse_tag(&obj.data).object),
            ObjType::Tree => {
                if let Ok(entries) = parse_tree(&obj.data) {
                    for e in entries {
                        if e.mode != "160000" {
                            queue.push(e.oid);
                        }
                    }
                }
            }
            ObjType::Blob => {}
        }
    }
    seen
}

struct FetchResult {
    objects: Vec<GitObject>,
    shallow: Vec<String>,
    unshallow: Vec<String>,
    acks: Vec<String>,
}

fn compute_fetch(store: &Arc<Store>, args: &FetchArgs) -> FetchResult {
    let lookup = Store::lookup_fn(store);
    let mut commit_roots: Vec<String> = Vec::new();
    let mut tag_objects: Vec<String> = Vec::new();
    for want in &args.wants {
        let obj = match lookup(want) {
            Some(o) => o,
            None => continue,
        };
        if obj.typ == ObjType::Tag {
            tag_objects.push(want.clone());
            let target = parse_tag(&obj.data).object;
            if let Some(t) = lookup(&target) {
                if t.typ == ObjType::Commit {
                    commit_roots.push(target);
                }
            }
        } else if obj.typ == ObjType::Commit {
            commit_roots.push(want.clone());
        }
    }
    // BFS over parents from the roots, tracking distance.
    let mut dist: HashMap<String, i64> = HashMap::new();
    let mut queue: std::collections::VecDeque<(String, i64)> = std::collections::VecDeque::new();
    for r in &commit_roots {
        queue.push_back((r.clone(), 0));
    }
    let mut bfs_commits: HashSet<String> = HashSet::new();
    while let Some((oid, d)) = queue.pop_front() {
        if dist.contains_key(&oid) {
            continue;
        }
        dist.insert(oid.clone(), d);
        let obj = match lookup(&oid) {
            Some(o) if o.typ == ObjType::Commit => o,
            _ => continue,
        };
        if args.has_depth && d >= args.depth {
            continue;
        }
        bfs_commits.insert(oid.clone());
        for p in &parse_commit(&obj.data).parents {
            queue.push_back((p.clone(), d + 1));
        }
    }
    let have_reachable = have_closure(&args.haves, &args.shallow, &lookup);
    let mut roots: Vec<String> = bfs_commits.iter().cloned().collect();
    roots.extend(tag_objects.iter().cloned());
    let want_reachable = shallow_closure(&roots, &lookup);
    let mut send_oids: Vec<String> = want_reachable
        .iter()
        .filter(|oid| !have_reachable.contains(*oid))
        .cloned()
        .collect();
    send_oids.sort();
    let mut objects: Vec<GitObject> = Vec::new();
    for oid in &send_oids {
        if let Some(obj) = lookup(oid) {
            objects.push((*obj).clone());
        }
    }
    let sent_commits: HashSet<String> = send_oids
        .iter()
        .filter(|oid| lookup(oid).map(|o| o.typ == ObjType::Commit).unwrap_or(false))
        .cloned()
        .collect();
    let mut acks: Vec<String> = Vec::new();
    for have in &args.haves {
        if lookup(have).map(|o| o.typ == ObjType::Commit).unwrap_or(false) {
            acks.push(have.clone());
        }
    }
    let mut shallow: Vec<String> = Vec::new();
    let mut unshallow: Vec<String> = Vec::new();
    if args.has_depth || !args.shallow.is_empty() {
        let mut held_after: HashSet<String> = HashSet::new();
        for oid in &sent_commits {
            held_after.insert(oid.clone());
        }
        for h in &args.haves {
            held_after.insert(h.clone());
        }
        for oid in &held_after {
            let obj = match lookup(oid) {
                Some(o) if o.typ == ObjType::Commit => o,
                _ => continue,
            };
            let parents = parse_commit(&obj.data).parents;
            if parents.is_empty() {
                continue;
            }
            for p in &parents {
                if !held_after.contains(p) {
                    shallow.push(oid.clone());
                    break;
                }
            }
        }
        shallow.sort();
        for oid in &args.shallow {
            let obj = match lookup(oid) {
                Some(o) if o.typ == ObjType::Commit => o,
                _ => continue,
            };
            let parents = parse_commit(&obj.data).parents;
            if parents.is_empty() {
                continue;
            }
            for p in &parents {
                if sent_commits.contains(p) {
                    unshallow.push(oid.clone());
                    break;
                }
            }
        }
        unshallow.sort();
    }
    FetchResult { objects, shallow, unshallow, acks }
}

fn sideband_chunks(pack: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in pack.chunks(32768) {
        let mut payload: Vec<u8> = Vec::with_capacity(chunk.len() + 1);
        payload.push(1);
        payload.extend_from_slice(chunk);
        out.extend_from_slice(&pkt_line(&payload));
    }
    out
}

fn handle_upload_pack(c: &Ctx, repo: &RepoState, is_v2: bool) -> crate::http::HttpResponse {
    let (packets, _rest) = decode_pkt_lines(c.body);
    let mut first: Option<&crate::gitobj::Pkt> = None;
    for p in &packets {
        if p.kind == PktKind::Data {
            first = Some(p);
            break;
        }
    }
    let first = match first {
        Some(f) => f,
        None => return c.err_json(400, "Bad request: no git command"),
    };
    let command = String::from_utf8_lossy(&c.body[first.start..first.end])
        .strip_suffix('\n')
        .map(|s| s.to_string())
        .unwrap_or_else(|| String::from_utf8_lossy(&c.body[first.start..first.end]).into_owned());
    let store = c.store;

    if command == "command=ls-refs" {
        let args = parse_ls_refs_args(&packets, c.body);
        let body = ls_refs_response(store, repo, &args);
        return git_resp_shared(c, "application/x-git-upload-pack-result", body);
    }

    if command == "command=fetch" {
        let args = parse_fetch_args(&packets, c.body);
        // The fetch response is cached per snapshot + the raw request body.
        // The cache mutex is held during computation: the clone workload's
        // 50 concurrent identical fetches compute once and hit 49 times.
        let mut cache = repo.fetch_cache.lock().unwrap();
        if let Some(cached) = cache.get(c.body) {
            let cached = cached.clone();
            drop(cache);
            return git_resp_shared(c, "application/x-git-upload-pack-result", cached);
        }
        let result = compute_fetch(store, &args);
        let mut body: Vec<u8> = Vec::new();
        if is_v2 {
            body.extend_from_slice(&pkt_line(b"acknowledgments\n"));
            if !result.acks.is_empty() {
                for ack in &result.acks {
                    body.extend_from_slice(&pkt_line(format!("ACK {}\n", ack).as_bytes()));
                }
                body.extend_from_slice(&pkt_line(b"ready\n"));
            } else {
                body.extend_from_slice(&pkt_line(b"NAK\n"));
            }
            body.extend_from_slice(b"0000");
            if !result.shallow.is_empty() || !result.unshallow.is_empty() {
                body.extend_from_slice(&pkt_line(b"shallow-info\n"));
                for oid in &result.shallow {
                    body.extend_from_slice(&pkt_line(format!("shallow {}\n", oid).as_bytes()));
                }
                for oid in &result.unshallow {
                    body.extend_from_slice(&pkt_line(format!("unshallow {}\n", oid).as_bytes()));
                }
                body.extend_from_slice(b"0000");
            }
            if !result.objects.is_empty() {
                let pack = write_pack(&result.objects);
                body.extend_from_slice(&pkt_line(b"packfile\n"));
                body.extend_from_slice(&sideband_chunks(&pack));
                body.extend_from_slice(b"0000");
            }
        } else {
            if !result.acks.is_empty() {
                for ack in &result.acks {
                    body.extend_from_slice(&pkt_line(format!("ACK {}\n", ack).as_bytes()));
                }
            } else {
                body.extend_from_slice(&pkt_line(b"NAK\n"));
            }
            for oid in &result.shallow {
                body.extend_from_slice(&pkt_line(format!("shallow {}\n", oid).as_bytes()));
            }
            for oid in &result.unshallow {
                body.extend_from_slice(&pkt_line(format!("unshallow {}\n", oid).as_bytes()));
            }
            if !result.objects.is_empty() {
                let pack = write_pack(&result.objects);
                body.extend_from_slice(&sideband_chunks(&pack));
            }
        }
        let body = Arc::new(body);
        cache.insert(c.body.to_vec(), body.clone());
        drop(cache);
        return git_resp_shared(c, "application/x-git-upload-pack-result", body);
    }

    c.err_json(400, "Bad request: unknown git command")
}

// --- receive-pack ---

struct PushCmd {
    old_oid: String,
    new_oid: String,
    ref_name: String,
}

fn handle_receive_pack(c: &Ctx, repo: &RepoState) -> crate::http::HttpResponse {
    let store = c.store;
    let (packets, rest) = decode_pkt_lines(c.body);
    let mut data_packets: Vec<&crate::gitobj::Pkt> = Vec::new();
    for p in &packets {
        if p.kind == PktKind::Data {
            data_packets.push(p);
        }
    }
    if data_packets.is_empty() {
        return c.err_json(400, "Bad request: no push commands");
    }
    let mut commands: Vec<PushCmd> = Vec::new();
    let mut malformed = false;
    for packet in &data_packets {
        let mut payload = &c.body[packet.start..packet.end];
        if !payload.is_empty() && payload[payload.len() - 1] == b'\n' {
            payload = &payload[..payload.len() - 1];
        }
        if let Some(i) = payload.iter().position(|&b| b == 0) {
            payload = &payload[..i];
        }
        let sp1 = payload.iter().position(|&b| b == b' ');
        let sp2 = sp1.and_then(|s| {
            payload[s + 1..]
                .iter()
                .position(|&b| b == b' ')
                .map(|j| s + 1 + j)
        });
        let ok = match (sp1, sp2) {
            (Some(a), Some(b)) => {
                is_oid_bytes(&payload[..a])
                    && is_oid_bytes(&payload[a + 1..b])
                    && payload[b + 1..].starts_with(b"refs/")
            }
            _ => false,
        };
        if !ok {
            malformed = true;
            break;
        }
        let (a, b) = (sp1.unwrap(), sp2.unwrap());
        commands.push(PushCmd {
            old_oid: String::from_utf8_lossy(&payload[..a]).into_owned(),
            new_oid: String::from_utf8_lossy(&payload[a + 1..b]).into_owned(),
            ref_name: String::from_utf8_lossy(&payload[b + 1..]).into_owned(),
        });
    }

    // The whole push (object insert + ref updates) runs under the repo lock
    // so concurrent pushes to one repo cannot interleave.
    let owner = repo.owner.clone();
    let name = repo.name.clone();
    let lookup = Store::lookup_fn(store);
    let (report_lines, unpack_ok) = store
        .mutate_repo(&owner, &name, |repo| {
            let mut report_lines: Vec<String> = Vec::new();
            let mut unpack_ok = true;
            if commands.is_empty() {
                unpack_ok = false;
                report_lines.push("unpack error no commands".to_string());
            } else if malformed {
                unpack_ok = false;
                report_lines.push("unpack error malformed command".to_string());
                for cmd in &commands {
                    report_lines.push(format!("ng {} malformed command", cmd.ref_name));
                }
            } else {
                let needs_pack = commands.iter().any(|cmd| cmd.new_oid != ZERO_OID);
                if needs_pack && !rest.is_empty() {
                    match read_pack(rest, &lookup) {
                        Ok(objects) => {
                            for (oid, obj) in objects {
                                if store.lookup(&oid).is_none() {
                                    store.put_object(obj.typ, obj.data);
                                }
                            }
                        }
                        Err(e) => {
                            unpack_ok = false;
                            report_lines.push(format!("unpack error {}", e));
                        }
                    }
                } else if needs_pack && rest.is_empty() {
                    unpack_ok = false;
                    report_lines.push("unpack error missing pack".to_string());
                }
                if unpack_ok {
                    report_lines.push("unpack ok".to_string());
                    let mut refs = (*repo.refs).clone();
                    for cmd in &commands {
                        let current = refs.map.get(&cmd.ref_name).cloned();
                        let exists = current.is_some();
                        if cmd.new_oid == ZERO_OID {
                            // delete
                            if !exists {
                                report_lines.push(format!("ng {} ref does not exist", cmd.ref_name));
                            } else if cmd.old_oid != ZERO_OID && cmd.old_oid != current.clone().unwrap() {
                                report_lines.push(format!("ng {} old value does not match", cmd.ref_name));
                            } else {
                                refs.del(&cmd.ref_name);
                                report_lines.push(format!("ok {}", cmd.ref_name));
                            }
                        } else if cmd.old_oid == ZERO_OID {
                            // create
                            if exists {
                                report_lines.push(format!("ng {} already exists", cmd.ref_name));
                            } else if store.lookup(&cmd.new_oid).is_none() {
                                report_lines.push(format!("ng {} missing object {}", cmd.ref_name, cmd.new_oid));
                            } else {
                                refs.set(&cmd.ref_name, &cmd.new_oid);
                                report_lines.push(format!("ok {}", cmd.ref_name));
                            }
                        } else {
                            // update
                            if !exists {
                                report_lines.push(format!("ng {} ref does not exist", cmd.ref_name));
                            } else if cmd.old_oid != current.clone().unwrap() {
                                report_lines.push(format!("ng {} old value does not match", cmd.ref_name));
                            } else if store.lookup(&cmd.new_oid).is_none() {
                                report_lines.push(format!("ng {} missing object {}", cmd.ref_name, cmd.new_oid));
                            } else if !is_descendant(&cmd.new_oid, &cmd.old_oid, &lookup) {
                                report_lines.push(format!("ng {} non-fast-forward", cmd.ref_name));
                            } else {
                                refs.set(&cmd.ref_name, &cmd.new_oid);
                                report_lines.push(format!("ok {}", cmd.ref_name));
                            }
                        }
                    }
                    let ts = store.next_ts();
                    let nr = repo.with_refs(Arc::new(refs), &crate::util::iso_of(ts));
                    return (Some(Arc::new(nr)), (report_lines, unpack_ok));
                } else {
                    for cmd in &commands {
                        report_lines.push(format!("ng {} unpack failed", cmd.ref_name));
                    }
                }
            }
            (None, (report_lines, unpack_ok))
        })
        .unwrap_or_else(|| (vec!["unpack error no commands".to_string()], false));

    let _ = unpack_ok;
    // Assemble the sideband report: pkt-line(0x01 + pkt-lines + flush) + flush.
    let mut inner: Vec<u8> = Vec::new();
    inner.push(1);
    for l in &report_lines {
        let mut line = l.clone();
        line.push('\n');
        inner.extend_from_slice(&pkt_line(line.as_bytes()));
    }
    inner.extend_from_slice(b"0000");
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&pkt_line(&inner));
    body.extend_from_slice(b"0000");
    git_resp_bytes(c, "application/x-git-receive-pack-result", body)
}
