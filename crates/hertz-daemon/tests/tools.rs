//! The conformance test every tool in the registry must pass, on a mock daemon
//! (127.0.0.1, ephemeral port). New tools are checked with no change here.
//!
//! - Names are unique snake_case; descriptions and schemas are present.
//! - `/mcp` `tools/list` and REST `/api/tools` list exactly the registry.
//! - Agent specs: a `when`, at least two options per question with unique
//!   labels, and every option builds arguments the tool accepts; the reply
//!   has every `{placeholder}` filled.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use hertz_daemon::ops::{self, OpError};
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tool_conforms() {
    let (daemon, listen, _tmp) = common::start().await;
    let state = daemon.state.clone();

    let mut names = HashSet::new();
    for t in ops::registry() {
        assert!(names.insert(t.name), "duplicate tool {}", t.name);
        assert!(
            t.name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{} is not snake_case",
            t.name
        );
        assert!(
            !t.description.trim().is_empty(),
            "{} has no description",
            t.name
        );
        let schema = (t.input_schema)(&state);
        assert_eq!(schema["type"], "object", "{}: schema type", t.name);
        assert!(
            schema["properties"].is_object(),
            "{}: schema properties",
            t.name
        );
    }

    // MCP and REST list the same tools as the registry, in order.
    let want: Vec<&str> = ops::registry().map(|t| t.name).collect();
    let (_, list) = common::rpc(
        &listen,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let mcp: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(mcp, want);
    let url = format!("http://{listen}/api/tools");
    let rest: Value =
        tokio::task::spawn_blocking(move || ureq::get(&url).call().unwrap().into_json().unwrap())
            .await
            .unwrap();
    assert_eq!(rest.as_array().unwrap().len(), want.len());

    for t in ops::registry() {
        let Some(spec) = t.agent else { continue };
        let spec = spec(&state);
        assert!(!spec.when.trim().is_empty(), "{}: empty when", t.name);
        assert!(!spec.reply.trim().is_empty(), "{}: empty reply", t.name);
        let mut args_seen = HashSet::new();
        for q in &spec.questions {
            assert!(
                args_seen.insert(q.arg.clone()),
                "{}: {} asked twice",
                t.name,
                q.arg
            );
            assert!(
                q.options.len() >= 2,
                "{}: {} needs two options",
                t.name,
                q.arg
            );
            let labels: HashSet<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
            assert_eq!(
                labels.len(),
                q.options.len(),
                "{}: {} labels repeat",
                t.name,
                q.arg
            );
        }
        // Each option of each question, with the other questions on their first
        // option, must build arguments the tool accepts and a complete reply.
        let first: BTreeMap<String, String> = spec
            .questions
            .iter()
            .map(|q| (q.arg.clone(), q.options[0].label.clone()))
            .collect();
        let mut combos = vec![first.clone()];
        for q in &spec.questions {
            for o in &q.options[1..] {
                let mut p = first.clone();
                p.insert(q.arg.clone(), o.label.clone());
                combos.push(p);
            }
        }
        for picks in combos {
            let args = spec
                .build_args(&picks)
                .unwrap_or_else(|e| panic!("{}: {e}", t.name));
            let out = tokio::time::timeout(
                Duration::from_secs(10),
                ops::call(&state, t.name, args.clone()),
            )
            .await
            .unwrap_or_else(|_| panic!("{} {args} took over 10 s", t.name));
            match out {
                Ok(result) => {
                    let reply = spec.fill_reply(&picks, &result);
                    assert!(
                        !reply.contains('{'),
                        "{} {args}: reply has an unfilled field: {reply:?}",
                        t.name
                    );
                }
                // Fine on the mock (no such hardware); a bad argument is not.
                Err(OpError::Unavailable(_)) => {}
                Err(e) => panic!("{} rejects its own agent arguments {args}: {e}", t.name),
            }
        }
    }

    daemon.shutdown();
    daemon.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_calls_any_tool() {
    let (daemon, listen, _tmp) = common::start().await;
    let post = |name: &str, body: Value| {
        let url = format!("http://{listen}/api/tools/{name}");
        tokio::task::spawn_blocking(move || match ureq::post(&url).send_json(body) {
            Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        })
    };
    let (s, body) = post("weather_status", json!({})).await.unwrap();
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["enabled"],
        false
    );
    let (s, body) = post("weather_set", json!({"enabled": true, "channels": ["72"]}))
        .await
        .unwrap();
    assert_eq!(s, 400);
    assert!(
        body.contains("WX1-WX7"),
        "the error carries the fix: {body}"
    );
    let (s, body) = post("no_such_tool", json!({})).await.unwrap();
    assert_eq!(s, 404);
    assert!(body.contains("weather_set"), "lists the tools: {body}");
    daemon.shutdown();
    daemon.join().await;
}
