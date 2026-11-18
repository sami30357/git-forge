// GraphQL v4: a small executor for exactly the documents the suite sends.
// Resolved values are built directly as Json; the gqlObj trees carry the
// type plus back-references to the (immutable) repo/issue/pr/comment they
// describe, so a mutation's payload is built from the post-mutation snapshot.

use crate::gitobj::diff_trees;
use crate::http::HttpResponse;
use crate::json::{push_json_string, write_json, Json, Parser};
use crate::server::Ctx;
use crate::store::{
    gid_int, gid_int2, new_issue, refresh_issue, sorted_issues, sorted_pulls, Issue,
    PullRequest, RepoState, Store, VIEWER_EMAIL, VIEWER_ID, VIEWER_LOGIN, VIEWER_NAME,
};
use crate::util::{b64decode, b64encode, iso_of};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum TokKind {
    Spread,
    Punct,
    Str,
    Int,
    Float,
    Name,
    Eof,
}

#[derive(Clone, Debug)]
struct Tok {
    kind: TokKind,
    value: String,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut tokens: Vec<Tok> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let ch = b[i];
        match ch {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => i += 1,
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'.' if i + 3 <= b.len() && &src[i..i + 3] == "..." => {
                tokens.push(Tok { kind: TokKind::Spread, value: "...".to_string() });
                i += 3;
            }
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b':' | b'!' | b'=' | b'$' | b'@'
            | b'&' | b'|' => {
                tokens.push(Tok { kind: TokKind::Punct, value: (ch as char).to_string() });
                i += 1;
            }
            b'"' => {
                let mut j = i + 1;
                let mut out: Vec<u8> = Vec::new();
                while j < b.len() && b[j] != b'"' {
                    if b[j] == b'\\' {
                        let esc = if j + 1 < b.len() { b[j + 1] } else { 0 };
                        match esc {
                            b'n' => out.push(b'\n'),
                            b't' => out.push(b'\t'),
                            b'r' => out.push(b'\r'),
                            b'b' => out.push(0x08),
                            b'f' => out.push(0x0c),
                            b'u' => {
                                let hex = std::str::from_utf8(&b[j + 2..(j + 6).min(b.len())])
                                    .unwrap_or("");
                                let n = u32::from_str_radix(hex, 16).unwrap_or(0);
                                let c = char::from_u32(n).unwrap_or('\u{FFFD}');
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                                j += 4;
                            }
                            _ => out.push(esc),
                        }
                        j += 2;
                    } else {
                        out.push(b[j]);
                        j += 1;
                    }
                }
                tokens.push(Tok { kind: TokKind::Str, value: String::from_utf8_lossy(&out).into_owned() });
                i = j + 1;
            }
            b'0'..=b'9' | b'-' => {
                let mut j = i;
                while j < b.len()
                    && (b[j].is_ascii_digit() || b[j] == b'.' || b[j] == b'e' || b[j] == b'E' || b[j] == b'+' || b[j] == b'-')
                {
                    j += 1;
                }
                let raw = &src[i..j];
                let kind = if raw.contains(['.', 'e', 'E']) { TokKind::Float } else { TokKind::Int };
                tokens.push(Tok { kind, value: raw.to_string() });
                i = j;
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let mut j = i;
                while j < b.len()
                    && (b[j].is_ascii_alphanumeric() || b[j] == b'_')
                {
                    j += 1;
                }
                tokens.push(Tok { kind: TokKind::Name, value: src[i..j].to_string() });
                i = j;
            }
            _ => return Err(format!("unexpected GraphQL character {:?}", (ch as char).to_string())),
        }
    }
    tokens.push(Tok { kind: TokKind::Eof, value: String::new() });
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum GVal {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
    Enum(String),
    Var(String),
    List(Vec<GVal>),
    Obj(Vec<(String, GVal)>),
}

#[derive(Clone, Debug)]
struct GField {
    alias: Option<String>,
    name: String,
    args: Vec<(String, GVal)>,
    selections: Vec<GSelection>,
    has_sel: bool,
}

#[derive(Clone, Debug)]
enum GSelection {
    Field(GField),
    Fragment { type_cond: String, sels: Vec<GSelection> },
}

#[derive(Clone, Debug)]
struct GOperation {
    kind: String, // query | mutation
    name: String,
    var_defs: Vec<(String, GVal)>,
    sels: Vec<GSelection>,
}

struct GParser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl GParser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos]
    }
    fn next(&mut self) -> Tok {
        let t = self.tokens[self.pos].clone();
        self.pos += 1;
        t
    }
    fn expect(&mut self, kind: TokKind, value: &str) -> Result<Tok, String> {
        let t = self.next();
        if t.kind != kind || (!value.is_empty() && t.value != value) {
            return Err(format!("expected {:?} {}, got {:?} {}", kind, value, t.kind, t.value));
        }
        Ok(t)
    }
    fn expect_name(&mut self, value: &str) -> Result<String, String> {
        let t = self.expect(TokKind::Name, "")?;
        if !value.is_empty() && t.value != value {
            return Err(format!("expected {}, got {}", value, t.value));
        }
        Ok(t.value)
    }

    fn parse_document(&mut self) -> Result<Vec<GOperation>, String> {
        let mut ops = Vec::new();
        while self.peek().kind != TokKind::Eof {
            ops.push(self.parse_operation()?);
        }
        Ok(ops)
    }

    fn parse_operation(&mut self) -> Result<GOperation, String> {
        let mut op = GOperation { kind: "query".to_string(), name: String::new(), var_defs: Vec::new(), sels: Vec::new() };
        let tok = self.peek().clone();
        if tok.kind == TokKind::Name && (tok.value == "query" || tok.value == "mutation") {
            op.kind = self.next().value;
            if self.peek().kind == TokKind::Name {
                op.name = self.next().value;
            }
        }
        if self.peek().kind == TokKind::Punct && self.peek().value == "(" {
            self.next();
            while !(self.peek().kind == TokKind::Punct && self.peek().value == ")") {
                self.expect(TokKind::Punct, "$")?;
                let var_name = self.expect_name("")?;
                self.expect(TokKind::Punct, ":")?;
                self.parse_type()?;
                if self.peek().kind == TokKind::Punct && self.peek().value == "=" {
                    self.next();
                    let def = self.parse_value()?;
                    op.var_defs.push((var_name, def));
                }
            }
            self.expect(TokKind::Punct, ")")?;
        }
        op.sels = self.parse_selection_set()?;
        Ok(op)
    }

    fn parse_type(&mut self) -> Result<(), String> {
        if self.peek().kind == TokKind::Punct && self.peek().value == "[" {
            self.next();
            self.parse_type()?;
            self.expect(TokKind::Punct, "]")?;
        } else {
            self.expect_name("")?;
        }
        if self.peek().kind == TokKind::Punct && self.peek().value == "!" {
            self.next();
        }
        Ok(())
    }

    fn parse_selection_set(&mut self) -> Result<Vec<GSelection>, String> {
        self.expect(TokKind::Punct, "{")?;
        let mut sels = Vec::new();
        while !(self.peek().kind == TokKind::Punct && self.peek().value == "}") {
            sels.push(self.parse_selection()?);
        }
        self.expect(TokKind::Punct, "}")?;
        Ok(sels)
    }

    fn parse_selection(&mut self) -> Result<GSelection, String> {
        if self.peek().kind == TokKind::Spread {
            self.next();
            self.expect_name("on")?;
            let type_cond = self.expect_name("")?;
            let sels = self.parse_selection_set()?;
            return Ok(GSelection::Fragment { type_cond, sels });
        }
        let mut name = self.expect_name("")?;
        let mut f = GField { alias: None, name: String::new(), args: Vec::new(), selections: Vec::new(), has_sel: false };
        if self.peek().kind == TokKind::Punct && self.peek().value == ":" {
            self.next();
            f.alias = Some(name);
            name = self.expect_name("")?;
        }
        f.name = name;
        if self.peek().kind == TokKind::Punct && self.peek().value == "(" {
            self.next();
            while !(self.peek().kind == TokKind::Punct && self.peek().value == ")") {
                let arg_name = self.expect_name("")?;
                self.expect(TokKind::Punct, ":")?;
                let val = self.parse_value()?;
                f.args.push((arg_name, val));
            }
            self.expect(TokKind::Punct, ")")?;
        }
        if self.peek().kind == TokKind::Punct && self.peek().value == "{" {
            f.selections = self.parse_selection_set()?;
            f.has_sel = true;
        }
        Ok(GSelection::Field(f))
    }

    fn parse_value(&mut self) -> Result<GVal, String> {
        let tok = self.next();
        match tok.kind {
            TokKind::Str => Ok(GVal::Str(tok.value)),
            TokKind::Int | TokKind::Float => {
                let f: f64 = tok.value.parse().unwrap_or(0.0);
                Ok(GVal::Num(f))
            }
            TokKind::Name => match tok.value.as_str() {
                "true" => Ok(GVal::Bool(true)),
                "false" => Ok(GVal::Bool(false)),
                "null" => Ok(GVal::Null),
                _ => Ok(GVal::Enum(tok.value)),
            },
            TokKind::Punct => match tok.value.as_str() {
                "$" => {
                    let name = self.expect_name("")?;
                    Ok(GVal::Var(name))
                }
                "[" => {
                    let mut items = Vec::new();
                    while !(self.peek().kind == TokKind::Punct && self.peek().value == "]") {
                        items.push(self.parse_value()?);
                    }
                    self.expect(TokKind::Punct, "]")?;
                    Ok(GVal::List(items))
                }
                "{" => {
                    let mut fields = Vec::new();
                    while !(self.peek().kind == TokKind::Punct && self.peek().value == "}") {
                        let name = self.expect_name("")?;
                        self.expect(TokKind::Punct, ":")?;
                        let val = self.parse_value()?;
                        fields.push((name, val));
                    }
                    self.expect(TokKind::Punct, "}")?;
                    Ok(GVal::Obj(fields))
                }
                _ => Err(format!("unexpected GraphQL value {:?} {}", tok.kind, tok.value)),
            },
            _ => Err(format!("unexpected GraphQL value {:?} {}", tok.kind, tok.value)),
        }
    }
}

fn eval_value(v: &GVal, variables: &[(String, Json)]) -> Json {
    match v {
        GVal::Str(s) => Json::Str(s.clone()),
        GVal::Num(n) => Json::Num(*n),
        GVal::Bool(b) => Json::Bool(*b),
        GVal::Null => Json::Null,
        GVal::Enum(s) => Json::Str(s.clone()),
        GVal::Var(name) => variables
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, val)| val.clone())
            .unwrap_or(Json::Null),
        GVal::List(items) => Json::Arr(items.iter().map(|i| eval_value(i, variables)).collect()),
        GVal::Obj(fields) => Json::Obj(
            fields.iter().map(|(k, val)| (k.clone(), eval_value(val, variables))).collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Schema tables
// ---------------------------------------------------------------------------

fn gql_fields(type_name: &str) -> &'static [&'static str] {
    match type_name {
        "Query" => &["viewer", "repository", "node", "__type"],
        "Mutation" => &["createIssue", "closeIssue", "createPullRequest", "mergePullRequest", "addComment"],
        "CreateIssuePayload" => &["issue"],
        "CloseIssuePayload" => &["issue"],
        "CreatePullRequestPayload" => &["pullRequest"],
        "MergePullRequestPayload" => &["pullRequest"],
        "AddCommentPayload" => &["commentEdge"],
        "Viewer" => &["login", "id", "name", "email", "databaseId", "avatarUrl"],
        "Repository" => &["id", "databaseId", "name", "nameWithOwner", "owner", "defaultBranchRef", "viewerPermission", "hasIssuesEnabled", "issue", "issues", "pullRequests", "pullRequest"],
        "User" => &["login", "id", "name", "email", "databaseId"],
        "Ref" => &["name", "target"],
        "Issue" => &["id", "number", "title", "body", "state", "url", "createdAt", "author"],
        "IssueConnection" => &["nodes", "edges", "pageInfo", "totalCount"],
        "IssueEdge" => &["node", "cursor"],
        "PullRequestEdge" => &["node", "cursor"],
        "PullRequestFileEdge" => &["node", "cursor"],
        "PageInfo" => &["hasNextPage", "hasPreviousPage", "startCursor", "endCursor"],
        "PullRequest" => &["id", "number", "title", "body", "state", "headRefName", "baseRefName", "mergeable", "merged", "files", "headRef", "baseRef", "author"],
        "PullRequestConnection" => &["nodes", "edges", "pageInfo", "totalCount"],
        "PullRequestFileConnection" => &["nodes", "edges", "pageInfo", "totalCount"],
        "PullRequestFile" => &["path", "additions", "deletions", "changeType"],
        "IssueComment" => &["id", "body", "author", "createdAt"],
        "IssueCommentEdge" => &["node", "cursor"],
        "IssueCommentConnection" => &["nodes", "edges", "pageInfo", "totalCount"],
        "__Type" => &["kind", "name", "description", "fields", "enumValues", "inputFields", "interfaces", "possibleTypes", "ofType"],
        "__Field" => &["name", "description", "args", "type", "isDeprecated", "deprecationReason"],
        "__InputValue" => &["name", "description", "type", "defaultValue"],
        "__EnumValue" => &["name", "description", "isDeprecated", "deprecationReason"],
        _ => &[],
    }
}

fn gql_enums(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "SearchType" => Some(&["ISSUE", "REPOSITORY", "USER", "CODE", "DISCUSSION", "COMMIT"]),
        "IssueState" => Some(&["OPEN", "CLOSED"]),
        "MergeableState" => Some(&["MERGEABLE", "CONFLICTING", "UNKNOWN"]),
        "PullRequestState" => Some(&["OPEN", "CLOSED", "MERGED"]),
        "PullRequestMergeMethod" => Some(&["MERGE", "SQUASH", "REBASE"]),
        "RepositoryPermission" => Some(&["ADMIN", "MAINTAIN", "WRITE", "TRIAGE", "READ"]),
        "IssueOrderField" => Some(&["CREATED_AT", "UPDATED_AT", "COMMENTS"]),
        "OrderDirection" => Some(&["ASC", "DESC"]),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum GFieldVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Obj(Arc<GObj>),
    Objs(Vec<Arc<GObj>>),
    Strs(Vec<String>),
}

impl GFieldVal {
    fn to_json(&self) -> Json {
        match self {
            GFieldVal::Null => Json::Null,
            GFieldVal::Bool(b) => Json::Bool(*b),
            GFieldVal::Num(n) => Json::Num(*n),
            GFieldVal::Str(s) => Json::Str(s.clone()),
            GFieldVal::Obj(_) => Json::Null, // resolved via resolve_field, never raw
            GFieldVal::Objs(_) => Json::Null,
            GFieldVal::Strs(items) => Json::Arr(items.iter().map(|s| Json::Str(s.clone())).collect()),
        }
    }
}

#[derive(Clone)]
struct GObj {
    typ: String,
    fields: Vec<(String, GFieldVal)>,
    repo: Option<Arc<RepoState>>,
    issue: Option<Arc<Issue>>,
    pr: Option<Arc<PullRequest>>,
    comment: Option<Arc<crate::store::Comment>>,
}

impl GObj {
    fn new(typ: &str) -> GObj {
        GObj { typ: typ.to_string(), fields: Vec::new(), repo: None, issue: None, pr: None, comment: None }
    }
    fn set(&mut self, key: &str, v: GFieldVal) {
        if let Some(slot) = self.fields.iter_mut().find(|(k, _)| k == key) {
            slot.1 = v;
        } else {
            self.fields.push((key.to_string(), v));
        }
    }
    fn get(&self, key: &str) -> Option<&GFieldVal> {
        self.fields.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    fn get_json(&self, key: &str) -> Json {
        match self.get(key) {
            Some(v) => v.to_json(),
            None => Json::Null,
        }
    }
}

fn decode_global_id(id: &str) -> Option<(String, Vec<String>)> {
    let text = String::from_utf8_lossy(&b64decode(id)).into_owned();
    let seg: Vec<&str> = text.split(':').collect();
    if seg.len() < 2 {
        return None;
    }
    Some((seg[0].to_string(), seg[1..].iter().map(|s| s.to_string()).collect()))
}

fn cursor_of(index: i64) -> String {
    b64encode(format!("cursor:{}", index).as_bytes())
}

fn index_of_cursor(cursor: &str) -> i64 {
    if cursor.is_empty() {
        return -1;
    }
    let text = String::from_utf8_lossy(&b64decode(cursor)).into_owned();
    let Some(rest) = text.strip_prefix("cursor:") else {
        return -1;
    };
    rest.parse::<i64>().unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

struct Executor<'a> {
    store: &'a Arc<Store>,
}

fn gql_error_response(msg: &str) -> Vec<u8> {
    let mut w: Vec<u8> = Vec::new();
    w.extend_from_slice(b"{\"data\":null,\"errors\":[{\"message\":");
    push_json_string(&mut w, msg);
    w.extend_from_slice(b"}]}");
    w
}

fn gql_data_response(data: &Json, errors: &[String]) -> Vec<u8> {
    let mut w: Vec<u8> = Vec::new();
    w.extend_from_slice(b"{\"data\":");
    write_json(&mut w, data);
    if !errors.is_empty() {
        w.extend_from_slice(b",\"errors\":[");
        for (i, msg) in errors.iter().enumerate() {
            if i > 0 {
                w.push(b',');
            }
            w.extend_from_slice(b"{\"message\":");
            push_json_string(&mut w, msg);
            w.push(b'}');
        }
        w.push(b']');
    }
    w.push(b'}');
    w
}

impl<'a> Executor<'a> {
    fn execute(&self, query: &str, variables: &[(String, Json)]) -> Vec<u8> {
        let tokens = match tokenize(query) {
            Ok(t) => t,
            Err(e) => return gql_error_response(&e),
        };
        let ops = match (GParser { tokens, pos: 0 }).parse_document() {
            Ok(o) => o,
            Err(e) => return gql_error_response(&e),
        };
        if ops.is_empty() {
            return gql_error_response("empty document");
        }
        let op = &ops[0];
        let mut errors: Vec<String> = Vec::new();
        let root_type = if op.kind == "mutation" { "Mutation" } else { "Query" };
        let mut data_fields: Vec<(String, Json)> = Vec::new();
        for sel in &op.sels {
            let GSelection::Field(f) = sel else { continue };
            let key = f.alias.clone().unwrap_or_else(|| f.name.clone());
            let (val, err_msg) = self.resolve_root(root_type, f, variables, &mut errors);
            if let Some(msg) = err_msg {
                errors.push(msg);
                data_fields.push((key, Json::Null));
            } else {
                data_fields.push((key, val));
            }
        }
        gql_data_response(&Json::Obj(data_fields), &errors)
    }

    fn eval_args(&self, f: &GField, variables: &[(String, Json)]) -> Vec<(String, Json)> {
        f.args.iter().map(|(k, v)| (k.clone(), eval_value(v, variables))).collect()
    }

    fn resolve_root(
        &self,
        root_type: &str,
        f: &GField,
        variables: &[(String, Json)],
        errors: &mut Vec<String>,
    ) -> (Json, Option<String>) {
        let args = self.eval_args(f, variables);
        if root_type == "Query" {
            match f.name.as_str() {
                "viewer" => {
                    return (self.resolve_fields("Viewer", &self.viewer_obj(), &f.selections, variables, errors), None);
                }
                "repository" => {
                    let owner = str_arg(&args, "owner");
                    let name = str_arg(&args, "name");
                    let repo = if owner == crate::store::VIEWER_LOGIN {
                        self.store.get_repo(&owner, &name)
                    } else {
                        None
                    };
                    return match repo {
                        Some(r) => (self.resolve_fields("Repository", &self.repo_obj(&r), &f.selections, variables, errors), None),
                        None => (Json::Null, None),
                    };
                }
                "node" => {
                    let id = str_arg(&args, "id");
                    let Some((kind, parts)) = decode_global_id(&id) else {
                        return (Json::Null, None);
                    };
                    let Some(obj) = self.node_obj(&kind, &parts) else {
                        return (Json::Null, None);
                    };
                    let typ = obj.typ.clone();
                    return (self.resolve_fields(&typ, &obj, &f.selections, variables, errors), None);
                }
                "__type" => {
                    let name = str_arg(&args, "name");
                    let Some(type_obj) = self.type_obj(&name) else {
                        return (Json::Null, None);
                    };
                    return (self.resolve_fields("__Type", &type_obj, &f.selections, variables, errors), None);
                }
                _ => {}
            }
        } else {
            match self.resolve_mutation(&f.name, &args) {
                MutationOut::Unknown => {
                    errors.push(format!("Cannot query field \"{}\" on type \"Mutation\".", f.name));
                    return (Json::Null, None);
                }
                MutationOut::None => return (Json::Null, None),
                MutationOut::Obj(mutation) => {
                    let typ = mutation.typ.clone();
                    return (self.resolve_fields(&typ, &mutation, &f.selections, variables, errors), None);
                }
            }
        }
        errors.push(format!("Cannot query field \"{}\" on type \"{}\".", f.name, root_type));
        (Json::Null, None)
    }

    fn viewer_obj(&self) -> GObj {
        let mut o = GObj::new("Viewer");
        o.set("login", GFieldVal::Str(VIEWER_LOGIN.to_string()));
        o.set("id", GFieldVal::Str(VIEWER_ID.to_string()));
        o.set("databaseId", GFieldVal::Num(VIEWER_ID as f64));
        o.set("name", GFieldVal::Str(VIEWER_NAME.to_string()));
        o.set("email", GFieldVal::Str(VIEWER_EMAIL.to_string()));
        o.set("avatarUrl", GFieldVal::Str(format!("https://example.invalid/avatar/{}", VIEWER_LOGIN)));
        o
    }

    fn user_obj(&self) -> GObj {
        let mut o = GObj::new("User");
        o.set("login", GFieldVal::Str(VIEWER_LOGIN.to_string()));
        o.set("id", GFieldVal::Str(VIEWER_ID.to_string()));
        o.set("databaseId", GFieldVal::Num(VIEWER_ID as f64));
        o.set("name", GFieldVal::Str(VIEWER_NAME.to_string()));
        o.set("email", GFieldVal::Str(VIEWER_EMAIL.to_string()));
        o
    }

    fn repo_obj(&self, r: &Arc<RepoState>) -> GObj {
        let mut o = GObj::new("Repository");
        o.repo = Some(r.clone());
        o.set("id", GFieldVal::Str(gid_int("Repository", r.id)));
        o.set("databaseId", GFieldVal::Num(r.id as f64));
        o.set("name", GFieldVal::Str(r.name.clone()));
        o.set("nameWithOwner", GFieldVal::Str(format!("{}/{}", r.owner, r.name)));
        o.set("owner", GFieldVal::Obj(Arc::new(self.user_obj())));
        let mut def_ref = GObj::new("Ref");
        def_ref.set("name", GFieldVal::Str(r.default_branch.clone()));
        let mut target = GObj::new("Commit");
        let oid = r.refs.map.get(&format!("refs/heads/{}", r.default_branch)).cloned().unwrap_or_default();
        target.set("oid", GFieldVal::Str(oid));
        def_ref.set("target", GFieldVal::Obj(Arc::new(target)));
        o.set("defaultBranchRef", GFieldVal::Obj(Arc::new(def_ref)));
        o.set("viewerPermission", GFieldVal::Str("ADMIN".to_string()));
        o.set("hasIssuesEnabled", GFieldVal::Bool(true));
        o
    }

    fn issue_obj(&self, r: &Arc<RepoState>, iss: &Arc<Issue>) -> GObj {
        let mut o = GObj::new("Issue");
        o.repo = Some(r.clone());
        o.issue = Some(iss.clone());
        o.set("id", GFieldVal::Str(gid_int2("Issue", r.id, iss.number)));
        o.set("number", GFieldVal::Num(iss.number as f64));
        o.set("title", GFieldVal::Str(iss.title.clone()));
        o.set("body", GFieldVal::Str(iss.body.clone()));
        o.set("state", GFieldVal::Str(iss.state.to_uppercase()));
        o.set("url", GFieldVal::Str(format!("https://example.invalid/{}/{}/issues/{}", r.owner, r.name, iss.number)));
        o.set("createdAt", GFieldVal::Str(iss.created_at.clone()));
        o.set("author", GFieldVal::Obj(Arc::new(self.user_obj())));
        o
    }

    fn pr_obj(&self, r: &Arc<RepoState>, pr: &Arc<PullRequest>) -> GObj {
        let base_tip = r
            .refs
            .map
            .get(&format!("refs/heads/{}", pr.base_ref))
            .cloned()
            .unwrap_or_else(|| pr.base_sha.clone());
        let head_tip = r
            .refs
            .map
            .get(&format!("refs/heads/{}", pr.head_ref))
            .cloned()
            .unwrap_or_else(|| pr.head_sha.clone());
        let mut state = pr.state.to_uppercase();
        if pr.merged {
            state = "MERGED".to_string();
        }
        let mergeable = if crate::store::pr_mergeable(self.store, r, pr) { "MERGEABLE" } else { "CONFLICTING" };
        let mut o = GObj::new("PullRequest");
        o.repo = Some(r.clone());
        o.pr = Some(pr.clone());
        o.set("id", GFieldVal::Str(gid_int2("PullRequest", r.id, pr.number)));
        o.set("number", GFieldVal::Num(pr.number as f64));
        o.set("title", GFieldVal::Str(pr.title.clone()));
        o.set("body", GFieldVal::Str(pr.body.clone()));
        o.set("state", GFieldVal::Str(state));
        o.set("headRefName", GFieldVal::Str(pr.head_ref.clone()));
        o.set("baseRefName", GFieldVal::Str(pr.base_ref.clone()));
        o.set("mergeable", GFieldVal::Str(mergeable.to_string()));
        o.set("merged", GFieldVal::Bool(pr.merged));
        let mut head_ref = GObj::new("Ref");
        head_ref.set("name", GFieldVal::Str(pr.head_ref.clone()));
        let mut head_target = GObj::new("Commit");
        head_target.set("oid", GFieldVal::Str(head_tip));
        head_ref.set("target", GFieldVal::Obj(Arc::new(head_target)));
        o.set("headRef", GFieldVal::Obj(Arc::new(head_ref)));
        let mut base_ref = GObj::new("Ref");
        base_ref.set("name", GFieldVal::Str(pr.base_ref.clone()));
        let mut base_target = GObj::new("Commit");
        base_target.set("oid", GFieldVal::Str(base_tip));
        base_ref.set("target", GFieldVal::Obj(Arc::new(base_target)));
        o.set("baseRef", GFieldVal::Obj(Arc::new(base_ref)));
        o.set("author", GFieldVal::Obj(Arc::new(self.user_obj())));
        o
    }

    fn comment_obj(&self, c: &Arc<crate::store::Comment>) -> GObj {
        let mut o = GObj::new("IssueComment");
        o.comment = Some(c.clone());
        o.set("id", GFieldVal::Str(gid_int("IssueComment", c.id)));
        o.set("body", GFieldVal::Str(c.body.clone()));
        o.set("createdAt", GFieldVal::Str(c.created_at.clone()));
        o.set("author", GFieldVal::Obj(Arc::new(self.user_obj())));
        o
    }

    fn node_obj(&self, kind: &str, parts: &[String]) -> Option<GObj> {
        let num = |i: usize| -> i64 {
            parts.get(i).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0)
        };
        match kind {
            "Repository" => {
                let r = self.store.get_repo_by_id(num(0))?;
                Some(self.repo_obj(&r))
            }
            "Issue" => {
                let r = self.store.get_repo_by_id(num(0))?;
                let iss = r.issues.get(&num(1))?.clone();
                Some(self.issue_obj(&r, &iss))
            }
            "PullRequest" => {
                let r = self.store.get_repo_by_id(num(0))?;
                let pr = r.pulls.get(&num(1))?.clone();
                Some(self.pr_obj(&r, &pr))
            }
            "IssueComment" => {
                for r in self.store.sorted_repos() {
                    for iss in r.issues.values() {
                        for c in &iss.comments {
                            if c.id == num(0) {
                                return Some(self.comment_obj(c));
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn type_obj(&self, name: &str) -> Option<GObj> {
        if !gql_fields(name).is_empty() {
            let mut o = GObj::new("__Type");
            o.set("kind", GFieldVal::Str("OBJECT".to_string()));
            o.set("name", GFieldVal::Str(name.to_string()));
            o.set("fields", GFieldVal::Strs(gql_fields(name).iter().map(|s| s.to_string()).collect()));
            o.set("enumValues", GFieldVal::Null);
            return Some(o);
        }
        if let Some(enums) = gql_enums(name) {
            let mut o = GObj::new("__Type");
            o.set("kind", GFieldVal::Str("ENUM".to_string()));
            o.set("name", GFieldVal::Str(name.to_string()));
            o.set("fields", GFieldVal::Null);
            o.set("enumValues", GFieldVal::Strs(enums.iter().map(|s| s.to_string()).collect()));
            return Some(o);
        }
        None
    }

    fn resolve_mutation(&self, name: &str, args: &[(String, Json)]) -> MutationOut {
        let input: Vec<(String, Json)> = match args.iter().find(|(k, _)| k == "input") {
            Some((_, Json::Obj(fields))) => fields.clone(),
            _ => Vec::new(),
        };
        let store = self.store;
        match name {
            "createIssue" => {
                let id = str_arg(&input, "repositoryId");
                let Some((kind, parts)) = decode_global_id(&id) else {
                    return MutationOut::None;
                };
                if kind != "Repository" {
                    return MutationOut::None;
                }
                let repo_id = parts.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let Some(repo) = store.get_repo_by_id(repo_id) else {
                    return MutationOut::None;
                };
                let title = str_arg(&input, "title");
                let body = str_arg(&input, "body");
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let number = r.issue_counter + 1;
                    let ts = store.next_ts();
                    let iss = new_issue(r, number, &title, &body, Vec::new(), ts);
                    let mut issues: HashMap<i64, Arc<Issue>> = (*r.issues).clone();
                    issues.insert(number, Arc::new(iss.clone()));
                    let mut nr = r.fork();
                    nr.issues = Arc::new(issues);
                    nr.issue_counter = number;
                    (Some(Arc::new(nr)), iss)
                });
                let Some(iss) = out else { return MutationOut::None };
                let r = store.get_repo(&repo.owner, &repo.name).unwrap_or(repo);
                let mut o = GObj::new("CreateIssuePayload");
                o.set("issue", GFieldVal::Obj(Arc::new(self.issue_obj(&r, &Arc::new(iss)))));
                MutationOut::Obj(o)
            }
            "closeIssue" => {
                let id = str_arg(&input, "issueId");
                let Some((kind, parts)) = decode_global_id(&id) else {
                    return MutationOut::None;
                };
                if kind != "Issue" {
                    return MutationOut::None;
                }
                let repo_id = parts.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let num = parts.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let Some(repo) = store.get_repo_by_id(repo_id) else {
                    return MutationOut::None;
                };
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let cur = match r.issues.get(&num) {
                        Some(i) => (**i).clone(),
                        None => return (None, None),
                    };
                    let mut iss = cur;
                    iss.state = "closed".to_string();
                    iss.closed_at = iso_of(store.next_ts());
                    iss.has_closed = true;
                    refresh_issue(r, &mut iss);
                    let mut issues: HashMap<i64, Arc<Issue>> = (*r.issues).clone();
                    issues.insert(num, Arc::new(iss.clone()));
                    let mut nr = r.fork();
                    nr.issues = Arc::new(issues);
                    (Some(Arc::new(nr)), Some(iss))
                });
                let Some(Some(iss)) = out else { return MutationOut::None };
                let r = store.get_repo(&repo.owner, &repo.name).unwrap_or(repo);
                let mut o = GObj::new("CloseIssuePayload");
                o.set("issue", GFieldVal::Obj(Arc::new(self.issue_obj(&r, &Arc::new(iss)))));
                MutationOut::Obj(o)
            }
            "createPullRequest" => {
                let id = str_arg(&input, "repositoryId");
                let Some((kind, parts)) = decode_global_id(&id) else {
                    return MutationOut::None;
                };
                if kind != "Repository" {
                    return MutationOut::None;
                }
                let repo_id = parts.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let Some(repo) = store.get_repo_by_id(repo_id) else {
                    return MutationOut::None;
                };
                let title = str_arg(&input, "title");
                let body = str_arg(&input, "body");
                let head = str_arg(&input, "headRefName");
                let base = str_arg(&input, "baseRefName");
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let head_sha = r.refs.map.get(&format!("refs/heads/{}", head)).cloned();
                    let base_sha = r.refs.map.get(&format!("refs/heads/{}", base)).cloned();
                    let (head_sha, base_sha) = match (head_sha, base_sha) {
                        (Some(h), Some(b)) => (h, b),
                        _ => return (None, None),
                    };
                    let number = r.pull_counter + 1;
                    let ts = store.next_ts();
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
                });
                let Some(Some(pr)) = out else { return MutationOut::None };
                let r = store.get_repo(&repo.owner, &repo.name).unwrap_or(repo);
                let mut o = GObj::new("CreatePullRequestPayload");
                o.set("pullRequest", GFieldVal::Obj(Arc::new(self.pr_obj(&r, &Arc::new(pr)))));
                MutationOut::Obj(o)
            }
            "mergePullRequest" => {
                let id = str_arg(&input, "pullRequestId");
                let Some((kind, parts)) = decode_global_id(&id) else {
                    return MutationOut::None;
                };
                if kind != "PullRequest" {
                    return MutationOut::None;
                }
                let repo_id = parts.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let num = parts.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let Some(repo) = store.get_repo_by_id(repo_id) else {
                    return MutationOut::None;
                };
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let cur = match r.pulls.get(&num) {
                        Some(p) => p.clone(),
                        None => return (None, false),
                    };
                    match Store::merge_pull_request(store, r, &cur) {
                        Ok((nr, _)) => (Some(nr), true),
                        Err(_) => (None, false),
                    }
                });
                let Some(true) = out else { return MutationOut::None };
                let r = store.get_repo(&repo.owner, &repo.name).unwrap_or(repo);
                let pr = match r.pulls.get(&num) {
                    Some(p) => p.clone(),
                    None => return MutationOut::None,
                };
                let mut o = GObj::new("MergePullRequestPayload");
                o.set("pullRequest", GFieldVal::Obj(Arc::new(self.pr_obj(&r, &pr))));
                MutationOut::Obj(o)
            }
            "addComment" => {
                let id = str_arg(&input, "subjectId");
                let Some((kind, parts)) = decode_global_id(&id) else {
                    return MutationOut::None;
                };
                if kind != "Issue" {
                    return MutationOut::None;
                }
                let repo_id = parts.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let num = parts.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
                let Some(repo) = store.get_repo_by_id(repo_id) else {
                    return MutationOut::None;
                };
                let body = str_arg(&input, "body");
                let out = store.mutate_repo(&repo.owner, &repo.name, |r| {
                    let cur = match r.issues.get(&num) {
                        Some(i) => (**i).clone(),
                        None => return (None, None),
                    };
                    let mut iss = cur;
                    let ts = store.next_ts();
                    let cm = Arc::new(crate::store::Comment {
                        id: store.next_comment_id(),
                        body: body.clone(),
                        created_at: iso_of(ts),
                        updated_at: iso_of(ts),
                    });
                    iss.comments.push(cm.clone());
                    refresh_issue(r, &mut iss);
                    let mut issues: HashMap<i64, Arc<Issue>> = (*r.issues).clone();
                    issues.insert(num, Arc::new(iss));
                    let mut nr = r.fork();
                    nr.issues = Arc::new(issues);
                    (Some(Arc::new(nr)), Some(cm))
                });
                let Some(Some(cm)) = out else { return MutationOut::None };
                let r = store.get_repo(&repo.owner, &repo.name).unwrap_or(repo);
                let iss = match r.issues.get(&num) {
                    Some(i) => i.clone(),
                    None => return MutationOut::None,
                };
                let mut edge = GObj::new("IssueCommentEdge");
                edge.set("node", GFieldVal::Obj(Arc::new(self.comment_obj(&cm))));
                edge.set("cursor", GFieldVal::Str(cursor_of((iss.comments.len() - 1) as i64)));
                let mut o = GObj::new("AddCommentPayload");
                o.set("commentEdge", GFieldVal::Obj(Arc::new(edge)));
                MutationOut::Obj(o)
            }
            _ => MutationOut::Unknown,
        }
    }

    fn resolve_fields(
        &self,
        type_name: &str,
        obj: &GObj,
        sels: &[GSelection],
        variables: &[(String, Json)],
        errors: &mut Vec<String>,
    ) -> Json {
        let mut out: Vec<(String, Json)> = Vec::new();
        for sel in sels {
            match sel {
                GSelection::Fragment { type_cond, sels: frag_sels } => {
                    if type_cond == type_name || type_cond == "Node" {
                        let nested = self.resolve_fields(type_name, obj, frag_sels, variables, errors);
                        if let Json::Obj(fields) = nested {
                            out.extend(fields);
                        }
                    }
                }
                GSelection::Field(f) => {
                    let key = f.alias.clone().unwrap_or_else(|| f.name.clone());
                    if !gql_fields(type_name).contains(&f.name.as_str()) {
                        errors.push(format!("Cannot query field \"{}\" on type \"{}\".", f.name, type_name));
                        out.push((key, Json::Null));
                        continue;
                    }
                    let val = self.resolve_field(type_name, obj, f, variables, errors);
                    out.push((key, val));
                }
            }
        }
        Json::Obj(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_field(
        &self,
        type_name: &str,
        obj: &GObj,
        f: &GField,
        variables: &[(String, Json)],
        errors: &mut Vec<String>,
    ) -> Json {
        let args = self.eval_args(f, variables);
        let sel = &f.selections;
        match type_name {
            "Viewer" | "User" => obj.get_json(&f.name),
            "Ref" => {
                if f.name == "target" {
                    let Some(GFieldVal::Obj(target)) = obj.get("target") else {
                        return Json::Null;
                    };
                    let mut out: Vec<(String, Json)> = Vec::new();
                    out.push(("oid".to_string(), target.get_json("oid")));
                    return Json::Obj(out);
                }
                obj.get_json(&f.name)
            }
            "Repository" => {
                let r = obj.repo.clone().unwrap();
                match f.name.as_str() {
                    "owner" => {
                        let Some(GFieldVal::Obj(owner)) = obj.get("owner") else {
                            return Json::Null;
                        };
                        self.resolve_fields("User", owner, sel, variables, errors)
                    }
                    "defaultBranchRef" => {
                        let Some(GFieldVal::Obj(def_ref)) = obj.get("defaultBranchRef") else {
                            return Json::Null;
                        };
                        self.resolve_fields("Ref", def_ref, sel, variables, errors)
                    }
                    "issue" => {
                        let iss = r.issues.get(&(num_arg(&args, "number") as i64));
                        match iss {
                            Some(iss) => self.resolve_fields("Issue", &self.issue_obj(&r, iss), sel, variables, errors),
                            None => Json::Null,
                        }
                    }
                    "issues" => self.issues_connection(&r, &args, sel, variables, errors),
                    "pullRequests" => self.prs_connection(&r, &args, sel, variables, errors),
                    "pullRequest" => {
                        let pr = r.pulls.get(&(num_arg(&args, "number") as i64));
                        match pr {
                            Some(pr) => self.resolve_fields("PullRequest", &self.pr_obj(&r, pr), sel, variables, errors),
                            None => Json::Null,
                        }
                    }
                    _ => obj.get_json(&f.name),
                }
            }
            "Issue" => obj.get_json(&f.name),
            "PullRequest" => {
                let r = obj.repo.clone().unwrap();
                let pr = obj.pr.clone().unwrap();
                match f.name.as_str() {
                    "files" => self.files_connection(&r, &pr, &args, sel, variables, errors),
                    "headRef" | "baseRef" => {
                        let Some(GFieldVal::Obj(ref_obj)) = obj.get(&f.name) else {
                            return Json::Null;
                        };
                        self.resolve_fields("Ref", ref_obj, sel, variables, errors)
                    }
                    "author" => {
                        let Some(GFieldVal::Obj(author)) = obj.get("author") else {
                            return Json::Null;
                        };
                        self.resolve_fields("User", author, sel, variables, errors)
                    }
                    _ => obj.get_json(&f.name),
                }
            }
            "IssueConnection" | "PullRequestConnection" | "PullRequestFileConnection" | "IssueCommentConnection" => {
                match f.name.as_str() {
                    "nodes" => {
                        let Some(GFieldVal::Objs(nodes)) = obj.get("nodes") else {
                            return Json::Null;
                        };
                        let mut out: Vec<Json> = Vec::new();
                        for n in nodes {
                            out.push(self.resolve_fields(&n.typ, n, sel, variables, errors));
                        }
                        Json::Arr(out)
                    }
                    "edges" => {
                        let Some(GFieldVal::Objs(nodes)) = obj.get("nodes") else {
                            return Json::Null;
                        };
                        let mut out: Vec<Json> = Vec::new();
                        for (i, n) in nodes.iter().enumerate() {
                            let edge_type = if n.typ == "Issue" {
                                "IssueEdge"
                            } else if n.typ == "PullRequest" {
                                "PullRequestEdge"
                            } else {
                                "PullRequestFileEdge"
                            };
                            let mut edge = GObj::new(edge_type);
                            edge.set("node", GFieldVal::Obj(n.clone()));
                            edge.set("cursor", GFieldVal::Str(cursor_of(i as i64)));
                            out.push(self.resolve_fields(edge_type, &edge, sel, variables, errors));
                        }
                        Json::Arr(out)
                    }
                    "pageInfo" => {
                        let Some(GFieldVal::Obj(pi)) = obj.get("pageInfo") else {
                            return Json::Null;
                        };
                        self.resolve_fields("PageInfo", pi, sel, variables, errors)
                    }
                    "totalCount" => {
                        let Some(GFieldVal::Objs(nodes)) = obj.get("nodes") else {
                            return Json::Null;
                        };
                        Json::Num(nodes.len() as f64)
                    }
                    _ => Json::Null,
                }
            }
            "IssueEdge" | "PullRequestEdge" | "PullRequestFileEdge" | "IssueCommentEdge" => {
                if f.name == "node" {
                    let Some(GFieldVal::Obj(node)) = obj.get("node") else {
                        return Json::Null;
                    };
                    let typ = node.typ.clone();
                    return self.resolve_fields(&typ, node, sel, variables, errors);
                }
                obj.get_json(&f.name)
            }
            "PullRequestFile" => obj.get_json(&f.name),
            "IssueComment" => {
                if f.name == "author" {
                    let Some(GFieldVal::Obj(author)) = obj.get("author") else {
                        return Json::Null;
                    };
                    return self.resolve_fields("User", author, sel, variables, errors);
                }
                obj.get_json(&f.name)
            }
            "__Type" => match f.name.as_str() {
                "fields" => {
                    let names = match obj.get("fields") {
                        Some(GFieldVal::Strs(items)) => items.clone(),
                        _ => Vec::new(),
                    };
                    let mut out: Vec<Json> = Vec::new();
                    for n in &names {
                        let mut field_obj = GObj::new("__Field");
                        field_obj.set("name", GFieldVal::Str(n.clone()));
                        field_obj.set("description", GFieldVal::Null);
                        field_obj.set("args", GFieldVal::Objs(Vec::new()));
                        field_obj.set("type", GFieldVal::Null);
                        field_obj.set("isDeprecated", GFieldVal::Bool(false));
                        field_obj.set("deprecationReason", GFieldVal::Null);
                        out.push(self.resolve_fields("__Field", &field_obj, sel, variables, errors));
                    }
                    Json::Arr(out)
                }
                "enumValues" => {
                    let values = match obj.get("enumValues") {
                        Some(GFieldVal::Strs(items)) => items.clone(),
                        _ => Vec::new(),
                    };
                    let mut out: Vec<Json> = Vec::new();
                    for n in &values {
                        let mut enum_obj = GObj::new("__EnumValue");
                        enum_obj.set("name", GFieldVal::Str(n.clone()));
                        enum_obj.set("description", GFieldVal::Null);
                        enum_obj.set("isDeprecated", GFieldVal::Bool(false));
                        enum_obj.set("deprecationReason", GFieldVal::Null);
                        out.push(self.resolve_fields("__EnumValue", &enum_obj, sel, variables, errors));
                    }
                    Json::Arr(out)
                }
                "inputFields" | "interfaces" | "possibleTypes" => Json::Arr(Vec::new()),
                _ => obj.get_json(&f.name),
            },
            "__Field" | "__EnumValue" | "__InputValue" => obj.get_json(&f.name),
            "CreateIssuePayload" | "CloseIssuePayload" => {
                if f.name == "issue" {
                    let Some(GFieldVal::Obj(iss)) = obj.get("issue") else {
                        return Json::Null;
                    };
                    return self.resolve_fields("Issue", iss, sel, variables, errors);
                }
                Json::Null
            }
            "CreatePullRequestPayload" | "MergePullRequestPayload" => {
                if f.name == "pullRequest" {
                    let Some(GFieldVal::Obj(pr)) = obj.get("pullRequest") else {
                        return Json::Null;
                    };
                    return self.resolve_fields("PullRequest", pr, sel, variables, errors);
                }
                Json::Null
            }
            "AddCommentPayload" => {
                if f.name == "commentEdge" {
                    let Some(GFieldVal::Obj(edge)) = obj.get("commentEdge") else {
                        return Json::Null;
                    };
                    return self.resolve_fields("IssueCommentEdge", edge, sel, variables, errors);
                }
                Json::Null
            }
            _ => obj.get_json(&f.name),
        }
    }

    fn issues_connection(
        &self,
        r: &Arc<RepoState>,
        args: &[(String, Json)],
        sel: &[GSelection],
        variables: &[(String, Json)],
        errors: &mut Vec<String>,
    ) -> Json {
        let mut list = sorted_issues(r);
        if let Some(Json::Arr(states)) = args.iter().find(|(k, _)| k == "states").map(|(_, v)| v) {
            if !states.is_empty() {
                let wanted: Vec<String> = states
                    .iter()
                    .filter_map(|s| s.as_str().map(|x| x.to_uppercase()))
                    .collect();
                list.retain(|iss| wanted.contains(&iss.state.to_uppercase()));
            }
        }
        if gql_str_field(args, "orderBy", "field") == "CREATED_AT"
            && gql_str_field(args, "orderBy", "direction") == "DESC"
        {
            list.reverse();
        }
        let nodes: Vec<Arc<GObj>> = list.iter().map(|iss| Arc::new(self.issue_obj(r, iss))).collect();
        self.connection(nodes, args, sel, variables, errors)
    }

    fn prs_connection(
        &self,
        r: &Arc<RepoState>,
        args: &[(String, Json)],
        sel: &[GSelection],
        variables: &[(String, Json)],
        errors: &mut Vec<String>,
    ) -> Json {
        let list = sorted_pulls(r);
        let nodes: Vec<Arc<GObj>> = list.iter().map(|pr| Arc::new(self.pr_obj(r, pr))).collect();
        self.connection(nodes, args, sel, variables, errors)
    }

    fn files_connection(
        &self,
        r: &Arc<RepoState>,
        pr: &Arc<PullRequest>,
        args: &[(String, Json)],
        sel: &[GSelection],
        variables: &[(String, Json)],
        errors: &mut Vec<String>,
    ) -> Json {
        let base_tip = r
            .refs
            .map
            .get(&format!("refs/heads/{}", pr.base_ref))
            .cloned()
            .unwrap_or_else(|| pr.base_sha.clone());
        let head_tip = r
            .refs
            .map
            .get(&format!("refs/heads/{}", pr.head_ref))
            .cloned()
            .unwrap_or_else(|| pr.head_sha.clone());
        let lookup = Store::lookup_fn(self.store);
        let files = diff_trees(&base_tip, &head_tip, &lookup);
        let nodes: Vec<Arc<GObj>> = files
            .iter()
            .map(|f| {
                let mut o = GObj::new("PullRequestFile");
                o.set("path", GFieldVal::Str(f.filename.clone()));
                o.set("additions", GFieldVal::Num(f.additions as f64));
                o.set("deletions", GFieldVal::Num(f.deletions as f64));
                o.set("changeType", GFieldVal::Str(f.status.to_uppercase()));
                Arc::new(o)
            })
            .collect();
        self.connection(nodes, args, sel, variables, errors)
    }

    fn connection(
        &self,
        nodes: Vec<Arc<GObj>>,
        args: &[(String, Json)],
        sel: &[GSelection],
        variables: &[(String, Json)],
        errors: &mut Vec<String>,
    ) -> Json {
        let mut after: i64 = -1;
        if let Some(Json::Str(s)) = args.iter().find(|(k, _)| k == "after").map(|(_, v)| v) {
            after = index_of_cursor(s);
        }
        let mut first = nodes.len() as i64;
        if let Some(Json::Num(f)) = args.iter().find(|(k, _)| k == "first").map(|(_, v)| v) {
            if *f > nodes.len() as f64 {
                first = nodes.len() as i64;
            } else if *f < 0.0 {
                first = 0;
            } else {
                first = *f as i64;
            }
        }
        let mut start = after + 1;
        if start < 0 {
            start = 0;
        }
        let mut end = start + first;
        if end > nodes.len() as i64 {
            end = nodes.len() as i64;
        }
        if start > nodes.len() as i64 {
            start = nodes.len() as i64;
            end = nodes.len() as i64;
        }
        let page: Vec<Arc<GObj>> = nodes[start as usize..end as usize].to_vec();
        let mut pi = GObj::new("PageInfo");
        pi.set("hasNextPage", GFieldVal::Bool((start as usize + page.len()) < nodes.len()));
        pi.set("hasPreviousPage", GFieldVal::Bool(start > 0));
        if page.is_empty() {
            pi.set("startCursor", GFieldVal::Null);
            pi.set("endCursor", GFieldVal::Null);
        } else {
            pi.set("startCursor", GFieldVal::Str(cursor_of(start)));
            pi.set("endCursor", GFieldVal::Str(cursor_of(start + page.len() as i64 - 1)));
        }
        let mut conn = GObj::new("IssueConnection");
        conn.set("nodes", GFieldVal::Objs(page));
        conn.set("pageInfo", GFieldVal::Obj(Arc::new(pi)));
        self.resolve_fields("IssueConnection", &conn, sel, variables, errors)
    }
}

enum MutationOut {
    Unknown,
    None,
    Obj(GObj),
}

fn str_arg(args: &[(String, Json)], key: &str) -> String {
    match args.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
        Some(Json::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

fn num_arg(args: &[(String, Json)], key: &str) -> i64 {
    match args.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
        Some(Json::Num(n)) => *n as i64,
        _ => 0,
    }
}

fn gql_str_field(args: &[(String, Json)], key: &str, sub: &str) -> String {
    match args.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
        Some(Json::Obj(fields)) => match fields.iter().find(|(k, _)| k == sub).map(|(_, v)| v) {
            Some(Json::Str(s)) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// HTTP handler
// ---------------------------------------------------------------------------

pub fn handle_graphql(c: &Ctx) -> HttpResponse {
    if !c.authorized() {
        return c.err_json(401, "Requires authentication");
    }
    let parsed = match Parser::new(c.body).parse_document() {
        Ok(v) => v,
        Err(()) => return c.err_json(400, "Problems parsing JSON"),
    };
    let executor = Executor { store: c.store };
    let run_one = |doc: &Json| -> Vec<u8> {
        let Json::Obj(fields) = doc else {
            return gql_error_response("invalid document");
        };
        let query = match fields.iter().find(|(k, _)| k == "query").map(|(_, v)| v) {
            Some(Json::Str(s)) => s.clone(),
            _ => String::new(),
        };
        let variables: Vec<(String, Json)> = match fields.iter().find(|(k, _)| k == "variables").map(|(_, v)| v) {
            Some(Json::Obj(vars)) => vars.clone(),
            _ => Vec::new(),
        };
        executor.execute(&query, &variables)
    };
    match &parsed {
        Json::Arr(docs) => {
            let mut w: Vec<u8> = Vec::new();
            w.push(b'[');
            for (i, doc) in docs.iter().enumerate() {
                if i > 0 {
                    w.push(b',');
                }
                w.extend_from_slice(&run_one(doc));
            }
            w.push(b']');
            c.json_bytes(200, w, &[])
        }
        _ => {
            let body = run_one(&parsed);
            c.json_bytes(200, body, &[])
        }
    }
}
