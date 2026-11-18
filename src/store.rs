// In-memory store: RCU snapshots + sharded object map + atomic counters.
//
// DIFFERENT FROM THE GO CORE BY DESIGN: the Go core holds one global mutex
// for the whole request. Here a repository is an immutable Arc<RepoState>;
// reads clone the Arc (one brief shared-lock hold on the repo map) and then
// serve with no locking at all. Mutations fork the snapshot (Arc fields
// clone cheaply; only the changed map is rebuilt) and swap the map entry
// under a short write lock, serialised per repo by a small mutex so two
// concurrent pushes to one repo cannot lose each other's ref update.
// Objects are append-only and live in 16 sharded maps, so the clone
// workload's 50 concurrent fetches never contend on a single lock.
// Counters are atomics. The serialised issue JSON is computed once when the
// issue is built and carried in the snapshot (zero work on the read path).

use crate::gitobj::{
    build_commit, merge_conflicts, parse_commit, GitObject, ObjType, TreeEntry,
};
use crate::json::{push_json_string, Json};
use crate::sha1::hash_object;
use crate::util::{b64encode, iso_of, itoa};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub const VIEWER_LOGIN: &str = "adaiversen";
pub const VIEWER_ID: i64 = 1000;
pub const VIEWER_NAME: &str = "ada iversen";
pub const VIEWER_EMAIL: &str = "adaiversen@example.invalid";
pub const BASE_TS: i64 = 1735689600; // 2025-01-01T00:00:00Z, the scenario epoch
pub const SERVER_NAME: &str = "forge";
pub const SERVER_VERSION: &str = "1.0.0";

#[derive(Clone)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: String,
}

#[derive(Clone)]
pub struct Comment {
    pub id: i64,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone)]
pub struct Issue {
    pub number: i64,
    pub id: i64,
    pub title: String,
    pub body: String,
    pub state: String, // open | closed
    pub labels: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: String,
    pub has_closed: bool,
    pub comments: Vec<Arc<Comment>>,
    pub cached: Vec<u8>, // serialised issue JSON, built with the issue
}

#[derive(Clone)]
pub struct PullRequest {
    pub number: i64,
    pub id: i64,
    pub title: String,
    pub body: String,
    pub state: String, // open | closed
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub base_sha: String,
    pub merged: bool,
    pub merged_at: String,
    pub merge_commit: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: String,
    pub has_closed: bool,
}

#[derive(Clone)]
pub struct RefMap {
    pub map: HashMap<String, String>,
    pub order: Vec<String>, // insertion order (REST /git/refs lists in this order)
}

impl RefMap {
    pub fn new() -> RefMap {
        RefMap { map: HashMap::new(), order: Vec::new() }
    }
    pub fn set(&mut self, name: &str, oid: &str) {
        if !self.map.contains_key(name) {
            self.order.push(name.to_string());
        }
        self.map.insert(name.to_string(), oid.to_string());
    }
    pub fn del(&mut self, name: &str) {
        if self.map.remove(name).is_some() {
            self.order.retain(|n| n != name);
        }
    }
}

pub struct RepoState {
    pub id: i64,
    pub name: String,
    pub owner: String,
    pub is_private: bool,
    pub description: String,
    pub default_branch: String,
    pub created_at: String,
    pub updated_at: String,
    pub pushed_at: String,
    pub refs: Arc<RefMap>,
    pub issues: Arc<HashMap<i64, Arc<Issue>>>,
    pub issue_counter: i64,
    pub pulls: Arc<HashMap<i64, Arc<PullRequest>>>,
    pub pull_counter: i64,
    pub labels: Arc<HashMap<String, Arc<Label>>>,
    pub label_counter: i64,
    pub gen: u64,
    // Per-snapshot caches. A snapshot's gen never changes, so the adv cache
    // needs no generation key; the ls/fetch caches are keyed by request.
    pub adv_up: Mutex<Option<Arc<Vec<u8>>>>,
    pub adv_recv: Mutex<Option<Arc<Vec<u8>>>>,
    pub ls_cache: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    pub fetch_cache: Mutex<HashMap<Vec<u8>, Arc<Vec<u8>>>>,
}

impl RepoState {
    pub fn new(id: i64, owner: &str, name: &str, is_private: bool, description: &str, ts: i64) -> RepoState {
        let iso = iso_of(ts);
        RepoState {
            id,
            name: name.to_string(),
            owner: owner.to_string(),
            is_private,
            description: description.to_string(),
            default_branch: "main".to_string(),
            created_at: iso.clone(),
            updated_at: iso.clone(),
            pushed_at: iso,
            refs: Arc::new(RefMap::new()),
            issues: Arc::new(HashMap::new()),
            issue_counter: 0,
            pulls: Arc::new(HashMap::new()),
            pull_counter: 0,
            labels: Arc::new(HashMap::new()),
            label_counter: 0,
            gen: 0,
            adv_up: Mutex::new(None),
            adv_recv: Mutex::new(None),
            ls_cache: Mutex::new(HashMap::new()),
            fetch_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fork for mutation: all Arc fields clone cheaply; caches start empty
    /// (a new snapshot's gen means the old cached bytes are stale anyway).
    pub fn fork(&self) -> RepoState {
        RepoState {
            id: self.id,
            name: self.name.clone(),
            owner: self.owner.clone(),
            is_private: self.is_private,
            description: self.description.clone(),
            default_branch: self.default_branch.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            pushed_at: self.pushed_at.clone(),
            refs: self.refs.clone(),
            issues: self.issues.clone(),
            issue_counter: self.issue_counter,
            pulls: self.pulls.clone(),
            pull_counter: self.pull_counter,
            labels: self.labels.clone(),
            label_counter: self.label_counter,
            gen: self.gen,
            adv_up: Mutex::new(None),
            adv_recv: Mutex::new(None),
            ls_cache: Mutex::new(HashMap::new()),
            fetch_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fork with a new ref map (a ref change bumps gen, invalidating caches).
    pub fn with_refs(&self, refs: Arc<RefMap>, ts_iso: &str) -> RepoState {
        let mut nr = self.fork();
        nr.refs = refs;
        nr.gen += 1;
        nr.updated_at = ts_iso.to_string();
        nr.pushed_at = ts_iso.to_string();
        nr
    }
}

pub struct Store {
    objects: [RwLock<HashMap<String, Arc<GitObject>>>; 16],
    repos: RwLock<HashMap<String, Arc<RepoState>>>,
    repo_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    repo_counter: AtomicI64,
    comment_id_counter: AtomicI64,
    ts_counter: AtomicI64,
    req_counter: AtomicU64,
}

pub fn repo_key(owner: &str, name: &str) -> String {
    format!("{}/{}", owner, name)
}

impl Store {
    pub fn new() -> Store {
        Store {
            objects: Default::default(),
            repos: RwLock::new(HashMap::new()),
            repo_locks: Mutex::new(HashMap::new()),
            repo_counter: AtomicI64::new(0),
            comment_id_counter: AtomicI64::new(0),
            ts_counter: AtomicI64::new(0),
            req_counter: AtomicU64::new(0),
        }
    }

    fn shard(oid: &str) -> usize {
        // The oid is hex; the low nibble of the first byte spreads evenly.
        let b = oid.as_bytes();
        let mut h: usize = 0;
        for &c in b.iter().take(8) {
            h = h.wrapping_mul(31).wrapping_add(c as usize);
        }
        h % 16
    }

    pub fn lookup(&self, oid: &str) -> Option<Arc<GitObject>> {
        self.objects[Self::shard(oid)].read().unwrap().get(oid).cloned()
    }

    /// Lookup closure for the git helpers. It owns an `Arc<Store>` so the
    /// closure is `'static` and can be handed to any helper.
    pub fn lookup_fn(store: &Arc<Store>) -> Box<dyn Fn(&str) -> Option<Arc<GitObject>> + Send + Sync> {
        let store = store.clone();
        Box::new(move |oid: &str| store.lookup(oid))
    }

    pub fn put_object(&self, typ: ObjType, data: Vec<u8>) -> String {
        let oid = hash_object(typ.as_str(), &data);
        let mut shard = self.objects[Self::shard(&oid)].write().unwrap();
        shard.entry(oid.clone()).or_insert_with(|| Arc::new(GitObject { typ, data }));
        oid
    }

    pub fn next_ts(&self) -> i64 {
        BASE_TS + self.ts_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn next_req_id(&self) -> String {
        let n = self.req_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("req-{:08x}", n)
    }

    pub fn get_repo(&self, owner: &str, name: &str) -> Option<Arc<RepoState>> {
        self.repos.read().unwrap().get(&repo_key(owner, name)).cloned()
    }

    pub fn get_repo_by_id(&self, id: i64) -> Option<Arc<RepoState>> {
        self.repos.read().unwrap().values().find(|r| r.id == id).cloned()
    }

    pub fn sorted_repos(&self) -> Vec<Arc<RepoState>> {
        let mut out: Vec<Arc<RepoState>> = self.repos.read().unwrap().values().cloned().collect();
        out.sort_by_key(|r| r.id);
        out
    }

    /// Create a repo; None when the name already exists on this account.
    pub fn create_repo(&self, owner: &str, name: &str, is_private: bool, description: &str) -> Option<Arc<RepoState>> {
        let key = repo_key(owner, name);
        let mut repos = self.repos.write().unwrap();
        if repos.contains_key(&key) {
            return None;
        }
        let id = self.repo_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let ts = self.next_ts();
        let repo = Arc::new(RepoState::new(id, owner, name, is_private, description, ts));
        repos.insert(key, repo.clone());
        Some(repo)
    }

    pub fn delete_repo(&self, owner: &str, name: &str) {
        self.repos.write().unwrap().remove(&repo_key(owner, name));
    }

    pub fn repo_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.repo_locks.lock().unwrap();
        locks.entry(key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    /// Serialised per-repo mutation: lock the repo, read the current
    /// snapshot, apply `f` (which returns the new snapshot, or None to abort
    /// without swapping), swap the map entry, return `f`'s second value.
    pub fn mutate_repo<T>(
        &self,
        owner: &str,
        name: &str,
        f: impl FnOnce(&Arc<RepoState>) -> (Option<Arc<RepoState>>, T),
    ) -> Option<T> {
        let key = repo_key(owner, name);
        let lock = self.repo_lock(&key);
        let _guard = lock.lock().unwrap();
        let repo = self.get_repo(owner, name)?;
        let (new_repo, out) = f(&repo);
        if let Some(nr) = new_repo {
            self.repos.write().unwrap().insert(key, nr);
        }
        Some(out)
    }

    /// Squash-merge a PR. Runs inside the caller's repo lock (use
    /// mutate_repo). Returns (new snapshot, merge oid) or a status string.
    pub fn merge_pull_request(
        store: &Arc<Store>,
        repo: &Arc<RepoState>,
        pr: &Arc<PullRequest>,
    ) -> Result<(Arc<RepoState>, String), &'static str> {
        if pr.state != "open" {
            return Err("already");
        }
        let base_tip = match repo.refs.map.get(&format!("refs/heads/{}", pr.base_ref)) {
            Some(t) => t.clone(),
            None => return Err("not-found"),
        };
        let head_tip = match repo.refs.map.get(&format!("refs/heads/{}", pr.head_ref)) {
            Some(t) => t.clone(),
            None => return Err("not-found"),
        };
        let lookup = Self::lookup_fn(store);
        if merge_conflicts(&base_tip, &head_tip, &lookup) {
            return Err("conflict");
        }
        let head_tree = store
            .lookup(&head_tip)
            .map(|o| parse_commit(&o.data).tree)
            .unwrap_or_default();
        let ts = store.next_ts();
        let message = format!(
            "Merge pull request #{} from {}/{}\n\n{}",
            pr.number, repo.owner, pr.head_ref, pr.title
        );
        let merge_oid = store.put_object(ObjType::Commit, build_commit(&head_tree, &[base_tip.clone()], &message, ts));
        let mut refs = (*repo.refs).clone();
        refs.set(&format!("refs/heads/{}", pr.base_ref), &merge_oid);
        let mut nr = repo.with_refs(Arc::new(refs), &iso_of(ts));
        let mut pulls: HashMap<i64, Arc<PullRequest>> = (*repo.pulls).clone();
        let mut npr = (**pr).clone();
        npr.state = "closed".to_string();
        npr.merged = true;
        npr.merged_at = iso_of(ts);
        npr.merge_commit = merge_oid.clone();
        npr.closed_at = iso_of(ts);
        npr.has_closed = true;
        npr.updated_at = iso_of(ts);
        npr.base_sha = merge_oid.clone();
        pulls.insert(pr.number, Arc::new(npr));
        nr.pulls = Arc::new(pulls);
        Ok((Arc::new(nr), merge_oid))
    }

    /// Create a commit on a branch (contents PUT/DELETE). Runs inside the
    /// caller's repo lock. Returns (new snapshot, commit oid).
    pub fn commit_on(
        &self,
        repo: &Arc<RepoState>,
        branch: &str,
        tree_oid: &str,
        parents: &[String],
        message: &str,
    ) -> (Arc<RepoState>, String) {
        let ts = self.next_ts();
        let commit_oid = self.put_object(
            ObjType::Commit,
            build_commit(tree_oid, parents, message, ts),
        );
        let mut refs = (*repo.refs).clone();
        refs.set(&format!("refs/heads/{}", branch), &commit_oid);
        let nr = repo.with_refs(Arc::new(refs), &iso_of(ts));
        (Arc::new(nr), commit_oid)
    }

    pub fn next_comment_id(&self) -> i64 {
        self.comment_id_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Create an issue (used by the MCP tools). Returns the created issue.
    pub fn create_issue(
        &self,
        owner: &str,
        name: &str,
        title: &str,
        body: &str,
        labels: Vec<String>,
    ) -> Option<Issue> {
        let title = title.to_string();
        let body = body.to_string();
        self.mutate_repo(owner, name, |r| {
            let number = r.issue_counter + 1;
            let ts = self.next_ts();
            let iss = new_issue(r, number, &title, &body, labels.clone(), ts);
            let mut issues: HashMap<i64, Arc<Issue>> = (*r.issues).clone();
            issues.insert(number, Arc::new(iss.clone()));
            let mut nr = r.fork();
            nr.issues = Arc::new(issues);
            nr.issue_counter = number;
            (Some(Arc::new(nr)), iss)
        })
    }

    /// Create a pull request (used by the MCP tools). None when a branch is missing.
    pub fn create_pull_request(
        &self,
        owner: &str,
        name: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Option<PullRequest> {
        let title = title.to_string();
        let body = body.to_string();
        let head = head.to_string();
        let base = base.to_string();
        self.mutate_repo(owner, name, |r| {
            let head_sha = r.refs.map.get(&format!("refs/heads/{}", head)).cloned();
            let base_sha = r.refs.map.get(&format!("refs/heads/{}", base)).cloned();
            let (head_sha, base_sha) = match (head_sha, base_sha) {
                (Some(h), Some(b)) => (h, b),
                _ => return (None, None),
            };
            let number = r.pull_counter + 1;
            let ts = self.next_ts();
            let pr = PullRequest {
                number,
                id: number,
                title: title.clone(),
                body: body.clone(),
                state: "open".to_string(),
                head_ref: head.clone(),
                base_ref: base.clone(),
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
        })
        .flatten()
    }

    /// Add a comment to an issue (used by the MCP tools via GraphQL; kept
    /// here for symmetry).
    pub fn add_comment(&self, owner: &str, name: &str, number: i64, body: &str) -> Option<Arc<Comment>> {
        let body = body.to_string();
        self.mutate_repo(owner, name, |r| {
            let cur = match r.issues.get(&number) {
                Some(i) => (**i).clone(),
                None => return (None, None),
            };
            let mut iss = cur;
            let ts = self.next_ts();
            let cm = Arc::new(Comment {
                id: self.next_comment_id(),
                body: body.clone(),
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
        })
        .flatten()
    }
}

// ---------------------------------------------------------------------------
// Sorting helpers
// ---------------------------------------------------------------------------

pub fn sorted_issues(r: &RepoState) -> Vec<Arc<Issue>> {
    let mut out: Vec<Arc<Issue>> = r.issues.values().cloned().collect();
    out.sort_by_key(|i| i.number);
    out
}

pub fn sorted_pulls(r: &RepoState) -> Vec<Arc<PullRequest>> {
    let mut out: Vec<Arc<PullRequest>> = r.pulls.values().cloned().collect();
    out.sort_by_key(|p| p.number);
    out
}

// ---------------------------------------------------------------------------
// Global ids
// ---------------------------------------------------------------------------

pub fn gid(parts: &[&str]) -> String {
    b64encode(parts.join(":").as_bytes())
}

pub fn gid_int(kind: &str, a: i64) -> String {
    b64encode(format!("{}:{}", kind, a).as_bytes())
}

pub fn gid_int2(kind: &str, a: i64, b: i64) -> String {
    b64encode(format!("{}:{}:{}", kind, a, b).as_bytes())
}

// ---------------------------------------------------------------------------
// JSON serializers (hand-rolled, field order matches the Node forge)
// ---------------------------------------------------------------------------

fn jw_str(out: &mut Vec<u8>, s: &str) {
    push_json_string(out, s);
}

pub fn user_json() -> Vec<u8> {
    let mut w: Vec<u8> = Vec::with_capacity(256);
    w.extend_from_slice(b"{\"login\":");
    jw_str(&mut w, VIEWER_LOGIN);
    w.extend_from_slice(b",\"id\":1000,\"node_id\":");
    jw_str(&mut w, &gid_int("User", VIEWER_ID));
    w.extend_from_slice(b",\"name\":");
    jw_str(&mut w, VIEWER_NAME);
    w.extend_from_slice(b",\"email\":");
    jw_str(&mut w, VIEWER_EMAIL);
    w.extend_from_slice(b",\"type\":\"User\",\"site_admin\":false,\"avatar_url\":");
    jw_str(&mut w, &format!("https://example.invalid/avatar/{}", VIEWER_LOGIN));
    w.extend_from_slice(b",\"html_url\":");
    jw_str(&mut w, &format!("https://example.invalid/{}", VIEWER_LOGIN));
    w.extend_from_slice(b",\"url\":");
    jw_str(&mut w, &format!("/api/v3/users/{}", VIEWER_LOGIN));
    w.extend_from_slice(b"}");
    w
}

pub fn repo_json(r: &RepoState) -> Vec<u8> {
    let mut w: Vec<u8> = Vec::with_capacity(512);
    w.extend_from_slice(b"{\"id\":");
    w.extend_from_slice(itoa(r.id).as_bytes());
    w.extend_from_slice(b",\"node_id\":");
    jw_str(&mut w, &gid_int("Repository", r.id));
    w.extend_from_slice(b",\"name\":");
    jw_str(&mut w, &r.name);
    w.extend_from_slice(b",\"full_name\":");
    jw_str(&mut w, &format!("{}/{}", r.owner, r.name));
    w.extend_from_slice(b",\"owner\":{\"login\":");
    jw_str(&mut w, &r.owner);
    w.extend_from_slice(b",\"id\":1000,\"type\":\"User\"},\"private\":");
    w.extend_from_slice(if r.is_private { b"true" } else { b"false" });
    w.extend_from_slice(b",\"description\":");
    jw_str(&mut w, &r.description);
    w.extend_from_slice(b",\"default_branch\":");
    jw_str(&mut w, &r.default_branch);
    w.extend_from_slice(b",\"created_at\":");
    jw_str(&mut w, &r.created_at);
    w.extend_from_slice(b",\"updated_at\":");
    jw_str(&mut w, &r.updated_at);
    w.extend_from_slice(b",\"pushed_at\":");
    jw_str(&mut w, &r.pushed_at);
    w.extend_from_slice(b",\"html_url\":");
    jw_str(&mut w, &format!("https://example.invalid/{}/{}", r.owner, r.name));
    w.extend_from_slice(b",\"url\":");
    jw_str(&mut w, &format!("/api/v3/repos/{}/{}", r.owner, r.name));
    w.extend_from_slice(b",\"fork\":false,\"has_issues\":true,\"has_projects\":true,\"has_wiki\":true}");
    w
}

pub fn label_json(l: &Label) -> Vec<u8> {
    let mut w: Vec<u8> = Vec::with_capacity(128);
    w.extend_from_slice(b"{\"id\":");
    w.extend_from_slice(itoa(l.id).as_bytes());
    w.extend_from_slice(b",\"node_id\":");
    jw_str(&mut w, &gid_int("Label", l.id));
    w.extend_from_slice(b",\"name\":");
    jw_str(&mut w, &l.name);
    w.extend_from_slice(b",\"color\":");
    jw_str(&mut w, &l.color);
    w.extend_from_slice(b",\"description\":\"\"}");
    w
}

/// Build an issue's serialised JSON (the bytes carried in Issue.cached).
pub fn build_issue_json(r: &RepoState, iss: &Issue) -> Vec<u8> {
    let mut w: Vec<u8> = Vec::with_capacity(512);
    w.extend_from_slice(b"{\"id\":");
    w.extend_from_slice(itoa(iss.id).as_bytes());
    w.extend_from_slice(b",\"node_id\":");
    jw_str(&mut w, &gid_int2("Issue", r.id, iss.number));
    w.extend_from_slice(b",\"number\":");
    w.extend_from_slice(itoa(iss.number).as_bytes());
    w.extend_from_slice(b",\"title\":");
    jw_str(&mut w, &iss.title);
    w.extend_from_slice(b",\"body\":");
    jw_str(&mut w, &iss.body);
    w.extend_from_slice(b",\"state\":");
    jw_str(&mut w, &iss.state);
    w.extend_from_slice(b",\"labels\":[");
    for (i, name) in iss.labels.iter().enumerate() {
        if i > 0 {
            w.push(b',');
        }
        if let Some(l) = r.labels.get(name) {
            w.extend_from_slice(&label_json(l));
        } else {
            w.extend_from_slice(b"{\"id\":0,\"name\":");
            jw_str(&mut w, name);
            w.extend_from_slice(b",\"color\":\"000000\",\"description\":\"\"}");
        }
    }
    w.extend_from_slice(b"],\"user\":{\"login\":");
    jw_str(&mut w, VIEWER_LOGIN);
    w.extend_from_slice(b",\"id\":1000,\"type\":\"User\"},\"assignee\":null,\"assignees\":[],\"created_at\":");
    jw_str(&mut w, &iss.created_at);
    w.extend_from_slice(b",\"updated_at\":");
    jw_str(&mut w, &iss.updated_at);
    w.extend_from_slice(b",\"closed_at\":");
    if iss.has_closed {
        jw_str(&mut w, &iss.closed_at);
    } else {
        w.extend_from_slice(b"null");
    }
    w.extend_from_slice(b",\"comments\":");
    w.extend_from_slice(itoa(iss.comments.len() as i64).as_bytes());
    w.extend_from_slice(b",\"html_url\":");
    jw_str(&mut w, &format!("https://example.invalid/{}/{}/issues/{}", r.owner, r.name, iss.number));
    w.extend_from_slice(b",\"url\":");
    jw_str(&mut w, &format!("/api/v3/repos/{}/{}/issues/{}", r.owner, r.name, iss.number));
    w.extend_from_slice(b"}");
    w
}

pub fn comment_json(r: &RepoState, iss: &Issue, c: &Comment) -> Vec<u8> {
    let mut w: Vec<u8> = Vec::with_capacity(256);
    w.extend_from_slice(b"{\"id\":");
    w.extend_from_slice(itoa(c.id).as_bytes());
    w.extend_from_slice(b",\"node_id\":");
    jw_str(&mut w, &gid_int("IssueComment", c.id));
    w.extend_from_slice(b",\"body\":");
    jw_str(&mut w, &c.body);
    w.extend_from_slice(b",\"user\":{\"login\":");
    jw_str(&mut w, VIEWER_LOGIN);
    w.extend_from_slice(b",\"id\":1000,\"type\":\"User\"},\"created_at\":");
    jw_str(&mut w, &c.created_at);
    w.extend_from_slice(b",\"updated_at\":");
    jw_str(&mut w, &c.updated_at);
    w.extend_from_slice(b",\"html_url\":");
    jw_str(&mut w, &format!("https://example.invalid/{}/{}/issues/{}#issuecomment-{}", r.owner, r.name, iss.number, c.id));
    w.extend_from_slice(b",\"url\":");
    jw_str(&mut w, &format!("/api/v3/repos/{}/{}/issues/{}/comments/{}", r.owner, r.name, iss.number, c.id));
    w.extend_from_slice(b"}");
    w
}

pub fn pr_mergeable(store: &Arc<Store>, r: &RepoState, pr: &PullRequest) -> bool {
    if pr.state != "open" {
        return true;
    }
    let base_tip = match r.refs.map.get(&format!("refs/heads/{}", pr.base_ref)) {
        Some(t) => t.clone(),
        None => return false,
    };
    let head_tip = match r.refs.map.get(&format!("refs/heads/{}", pr.head_ref)) {
        Some(t) => t.clone(),
        None => return false,
    };
    let lookup = Store::lookup_fn(store);
    !merge_conflicts(&base_tip, &head_tip, &lookup)
}

pub fn pr_json(store: &Arc<Store>, r: &RepoState, pr: &PullRequest) -> Vec<u8> {
    let head_tip = r
        .refs
        .map
        .get(&format!("refs/heads/{}", pr.head_ref))
        .cloned()
        .unwrap_or_else(|| pr.head_sha.clone());
    let base_tip = r
        .refs
        .map
        .get(&format!("refs/heads/{}", pr.base_ref))
        .cloned()
        .unwrap_or_else(|| pr.base_sha.clone());
    let mergeable = pr_mergeable(store, r, pr);
    let mut w: Vec<u8> = Vec::with_capacity(640);
    w.extend_from_slice(b"{\"id\":");
    w.extend_from_slice(itoa(pr.id).as_bytes());
    w.extend_from_slice(b",\"node_id\":");
    jw_str(&mut w, &gid_int2("PullRequest", r.id, pr.number));
    w.extend_from_slice(b",\"number\":");
    w.extend_from_slice(itoa(pr.number).as_bytes());
    w.extend_from_slice(b",\"state\":");
    jw_str(&mut w, &pr.state);
    w.extend_from_slice(b",\"title\":");
    jw_str(&mut w, &pr.title);
    w.extend_from_slice(b",\"body\":");
    jw_str(&mut w, &pr.body);
    w.extend_from_slice(b",\"merged\":");
    w.extend_from_slice(if pr.merged { b"true" } else { b"false" });
    w.extend_from_slice(b",\"merged_at\":");
    if pr.merged_at.is_empty() {
        w.extend_from_slice(b"null");
    } else {
        jw_str(&mut w, &pr.merged_at);
    }
    w.extend_from_slice(b",\"merge_commit_sha\":");
    if pr.merge_commit.is_empty() {
        w.extend_from_slice(b"null");
    } else {
        jw_str(&mut w, &pr.merge_commit);
    }
    w.extend_from_slice(b",\"mergeable\":");
    w.extend_from_slice(if mergeable { b"true" } else { b"false" });
    w.extend_from_slice(b",\"mergeable_state\":");
    if pr.state != "open" {
        jw_str(&mut w, "unknown");
    } else if mergeable {
        jw_str(&mut w, "clean");
    } else {
        jw_str(&mut w, "dirty");
    }
    w.extend_from_slice(b",\"head\":{\"ref\":");
    jw_str(&mut w, &pr.head_ref);
    w.extend_from_slice(b",\"sha\":");
    jw_str(&mut w, &head_tip);
    w.extend_from_slice(b",\"label\":");
    jw_str(&mut w, &format!("{}:{}", r.owner, pr.head_ref));
    w.extend_from_slice(b"},\"base\":{\"ref\":");
    jw_str(&mut w, &pr.base_ref);
    w.extend_from_slice(b",\"sha\":");
    jw_str(&mut w, &base_tip);
    w.extend_from_slice(b",\"label\":");
    jw_str(&mut w, &format!("{}:{}", r.owner, pr.base_ref));
    w.extend_from_slice(b"},\"user\":{\"login\":");
    jw_str(&mut w, VIEWER_LOGIN);
    w.extend_from_slice(b",\"id\":1000,\"type\":\"User\"},\"created_at\":");
    jw_str(&mut w, &pr.created_at);
    w.extend_from_slice(b",\"updated_at\":");
    jw_str(&mut w, &pr.updated_at);
    w.extend_from_slice(b",\"closed_at\":");
    if pr.has_closed {
        jw_str(&mut w, &pr.closed_at);
    } else {
        w.extend_from_slice(b"null");
    }
    w.extend_from_slice(b",\"html_url\":");
    jw_str(&mut w, &format!("https://example.invalid/{}/{}/pull/{}", r.owner, r.name, pr.number));
    w.extend_from_slice(b",\"url\":");
    jw_str(&mut w, &format!("/api/v3/repos/{}/{}/pulls/{}", r.owner, r.name, pr.number));
    w.extend_from_slice(b"}");
    w
}

/// Build a fresh Issue (with its cached JSON) for a repo snapshot.
pub fn new_issue(r: &RepoState, number: i64, title: &str, body: &str, labels: Vec<String>, ts: i64) -> Issue {
    let iso = iso_of(ts);
    let mut iss = Issue {
        number,
        id: number,
        title: title.to_string(),
        body: body.to_string(),
        state: "open".to_string(),
        labels,
        created_at: iso.clone(),
        updated_at: iso,
        closed_at: String::new(),
        has_closed: false,
        comments: Vec::new(),
        cached: Vec::new(),
    };
    iss.cached = build_issue_json(r, &iss);
    iss
}

/// Rebuild an issue's cached JSON after a mutation (state/title/body/labels/comments).
pub fn refresh_issue(r: &RepoState, iss: &mut Issue) {
    iss.cached = build_issue_json(r, iss);
}

// Re-export for handlers that build Json values.
pub use crate::json::Json as JVal;
pub use crate::json::write_json as write_jval;

// Silence an unused import in some build configurations.
#[allow(dead_code)]
fn _unused(_: TreeEntry) {}
