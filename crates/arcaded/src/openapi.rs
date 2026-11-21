//! The web API described for machines (ARCADE #25 part 2): an OpenAPI 3.1 document made from the one command table
//! (crate::commands: names, permissions, typed parameters, answer shapes), served at GET /api/v1/openapi.json and
//! by `rpc.openapi`; and the short guide for agents and scripts (agents.md), at GET /api/v1/agents.md and `rpc.guide`.

use crate::commands::{self, Cmd, Perm};
use serde_json::{json, Map, Value};

/// The guide for AI agents and scripts.
pub const GUIDE: &str = include_str!("agents.md");

/// A JSON Schema for a parameter type ("integer|string": either).
fn type_schema(ty: &str) -> Value {
    let types: Vec<&str> = ty.split('|').collect();
    let one = |t: &str| match t {
        "array" => json!({"type": "array", "items": {}}),
        t => json!({"type": t}),
    };
    if types.len() == 1 {
        one(types[0])
    } else {
        json!({"anyOf": types.iter().map(|t| one(t)).collect::<Vec<_>>()})
    }
}

fn param_schema(p: &Value) -> Value {
    let mut s = type_schema(p["type"].as_str().unwrap_or("string"));
    let mut d = p["description"].as_str().unwrap_or("").to_string();
    if let Some(n) = p["note"].as_str() {
        d = format!("{d} ({n})").trim().to_string();
    }
    if let Some(g) = p["one_of"].as_array() {
        let names: Vec<&str> = g.iter().filter_map(|x| x.as_str()).collect();
        d = format!("{d} One of: {}{}.", names.join(", "), if p["one_of_required"] == true { " (one is required)" } else { "" }).trim().to_string();
    }
    s["description"] = json!(d);
    for k in ["enum", "default", "maximum"] {
        if !p[k].is_null() {
            s[k] = p[k].clone();
        }
    }
    s
}

fn body_schema(c: &Cmd) -> Value {
    let ps = commands::params_of(c);
    let mut props = Map::new();
    let mut required = Vec::new();
    for p in &ps {
        let name = p["name"].as_str().unwrap_or("").to_string();
        if p["required"] == true {
            required.push(json!(name));
        }
        props.insert(name, param_schema(p));
    }
    let mut s = json!({"type": "object", "properties": props, "additionalProperties": true});
    if !required.is_empty() {
        s["required"] = json!(required);
    }
    s
}

fn first_sentence(s: &str) -> String {
    let cut = s.find(". ").map(|i| i + 1).unwrap_or(s.len());
    s[..cut].chars().take(160).collect()
}

fn operation(c: &Cmd, get: bool) -> Value {
    let id = format!("{}{}", c.name.replace('.', "_"), if get { "_get" } else { "" });
    let mut op = json!({
        "operationId": id,
        "summary": first_sentence(c.summary),
        "description": c.summary,
        "tags": [c.name.split('.').next().unwrap_or(c.name)],
        "x-rpc-method": c.name,
        "x-permission": c.perm.name(),
        "x-sends": commands::is_send(c),
        "x-result-shape": commands::result_of(c)["shape"],
        "responses": {
            "200": {"description": "Answered", "content": {"application/json": {"schema": {"type": "object", "required": ["ok", "result"],
                "properties": {"ok": {"const": true}, "result": {"description": commands::result_of(c)["shape"]}}}}}},
            "400": {"$ref": "#/components/responses/Error"},
            "401": {"$ref": "#/components/responses/Error"},
            "403": {"$ref": "#/components/responses/Error"},
            "404": {"$ref": "#/components/responses/Error"},
            "503": {"$ref": "#/components/responses/NotReady"},
        },
    });
    op["security"] = match c.perm {
        Perm::Public => json!([]),
        _ => json!([{"apiKey": []}]),
    };
    if c.perm == Perm::Local {
        op["x-local-only"] = json!(true);
        op["description"] = json!(format!("{} Local: only this computer's socket or the cookie JSON-RPC (/rpc) may call it; an API key gets 403 forbidden.", c.summary));
    }
    if get {
        let ps = commands::params_of(c);
        op["parameters"] = json!(ps.iter().map(|p| json!({"name": p["name"], "in": "query", "required": p["required"], "schema": param_schema(p)})).collect::<Vec<_>>());
    } else {
        op["requestBody"] = json!({"required": false, "content": {"application/json": {"schema": body_schema(c)}}});
    }
    op
}

/// The OpenAPI 3.1 document.
pub fn document() -> Value {
    document_for(None)
}

/// The OpenAPI 3.1 document, only the paths of commands whose names start with `prefix` when one is given
/// (2026-11-21, ARCADE #13: the whole document is about 800 KB). The components (the error answers and the API key
/// scheme) are small and every path refers to them, so they always come whole.
pub fn document_for(prefix: Option<&str>) -> Value {
    let mut paths = Map::new();
    for c in commands::COMMANDS.iter().filter(|c| commands::matches(c, None, prefix)) {
        // POST for every command (a JSON body); GET too for reads (query fields: numbers and true/false are parsed).
        let mut item = json!({"post": operation(c, false)});
        if matches!(c.perm, Perm::Public | Perm::Read) {
            item["get"] = operation(c, true);
        }
        paths.insert(commands::route_of(c.name), item);
    }
    let codes: Vec<&str> = crate::api::ERROR_CODES.to_vec();
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Arcade V1.0 API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Every command of an arcade, the same on the unix socket ({\"cmd\": name, ...args}), JSON-RPC 2.0 (POST /rpc, method = name, params = args, cookie Basic auth) and this web API (POST or GET /api/v1/<name with / for .>, Authorization: Bearer ak_...). Generated from the arcade's command table. The guide for agents and scripts: GET /api/v1/agents.md.",
        },
        "servers": [{"url": "/"}],
        "paths": paths,
        "components": {
            "securitySchemes": {
                "apiKey": {"type": "http", "scheme": "bearer", "description": "An API key (ak_<id>_<secret>) made on this computer with `arcade apikey create` (apikey.create), with permissions read, message, post, tip, wallet or admin. Public commands need none."},
            },
            "schemas": {
                "Error": {"type": "object", "required": ["ok", "error", "code"], "properties": {
                    "ok": {"const": false}, "error": {"type": "string", "description": "What went wrong, in words (may change between versions)."},
                    "code": {"type": "string", "enum": codes, "description": "Stable: match on this, not on error."}}},
                "NotReady": {"type": "object", "properties": {"ok": {"const": false}, "state": {"enum": ["starting", "stopping"]}, "code": {"enum": ["starting", "stopping"]}, "retry_after_s": {"type": "integer"}}},
            },
            "responses": {
                "Error": {"description": "Refused or failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                "NotReady": {"description": "Starting or stopping: retry after Retry-After seconds", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/NotReady"}}}},
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_names_every_command_once() {
        let d = document();
        assert_eq!(d["openapi"], "3.1.0");
        let paths = d["paths"].as_object().unwrap();
        assert_eq!(paths.len(), commands::COMMANDS.len(), "one path per command");
        let mut ids = std::collections::BTreeSet::new();
        for (route, item) in paths {
            for (_, op) in item.as_object().unwrap() {
                assert!(ids.insert(op["operationId"].as_str().unwrap().to_string()), "operationId twice: {route}");
            }
        }
        let w = &paths["/api/v1/tx/wait"]["post"];
        let until = &w["requestBody"]["content"]["application/json"]["schema"]["properties"]["until"];
        assert_eq!(until["enum"][3], "confirmed");
        assert_eq!(until["default"], "confirmed");
        let send = &paths["/api/v1/wallet/send"]["post"];
        assert_eq!(send["x-local-only"], true);
        assert_eq!(send["requestBody"]["content"]["application/json"]["schema"]["properties"]["client_ref"]["type"], "string");
        assert!(paths["/api/v1/explorer/blocks"]["get"]["parameters"].as_array().unwrap().iter().any(|p| p["name"] == "order"));
        assert_eq!(paths["/api/v1/status"]["get"]["security"], json!([]));
        assert_eq!(paths["/api/v1/ledger/item_open"]["post"]["security"], json!([{"apiKey": []}]));
    }

    /// 2026-11-21 (ARCADE #13): a prefix keeps only those commands' paths, and the components they refer to.
    #[test]
    fn a_prefix_keeps_only_its_paths() {
        let all = document();
        assert_eq!(document_for(None), all, "no prefix: the same document as before");
        let d = document_for(Some("ledger.item"));
        let paths = d["paths"].as_object().unwrap();
        assert!(paths.len() < all["paths"].as_object().unwrap().len());
        assert!(paths.contains_key("/api/v1/ledger/items") && paths.contains_key("/api/v1/ledger/item_open"));
        assert!(paths.keys().all(|k| k.starts_with("/api/v1/ledger/item")));
        assert_eq!(d["components"], all["components"]);
        // Every $ref the kept paths use resolves in the components.
        let text = d["paths"].to_string();
        for r in text.match_indices("#/components/").map(|(i, _)| text[i + 2..].split('"').next().unwrap()) {
            let mut at = &d;
            for part in r.split('/') {
                at = &at[part];
            }
            assert!(!at.is_null(), "{r} is missing");
        }
        let tx = document_for(Some("tx."));
        println!("rpc.openapi: {} bytes; prefix tx.: {}; prefix ledger.item: {}", all.to_string().len(), tx.to_string().len(), d.to_string().len());
    }

    #[test]
    fn the_guide_covers_what_an_agent_needs() {
        for want in ["client_ref", "tx.wait", "cursor", "code", "decimals", "openapi.json", "events.since", "chains.wait", "Bearer", "service.stop", "local"] {
            assert!(GUIDE.contains(want), "the guide says nothing of {want}");
        }
        for (name, _) in commands::PRIVATE {
            assert!(GUIDE.contains(name), "the guide's private list misses {name}");
        }
    }
}
