use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn main() {
    match std::env::args().nth(1).as_deref() {
        None => pty_lsp(),
        Some("framed-lsp") => framed_lsp(),
        Some("pty-lsp") => pty_lsp(),
        Some("descendant") => descendant(),
        Some("ready-descendant") => ready_descendant(),
        Some("utf8-initialize") => incompatible_initialize(),
        Some("crash-initialize") => crash_initialize(),
        Some("timeout-navigation") => timeout_navigation(),
        Some("pty-resilience") => pty_resilience(),
        Some("session-reuse") => session_reuse_lsp(),
        Some("stalled-session-tree") => stalled_session_tree(),
        #[cfg(unix)]
        Some("escaped-pipe-owner") => escaped_pipe_owner(),
        Some("pipe-holder") => std::thread::sleep(Duration::from_secs(30)),
        other => panic!("unknown LSP test-helper role: {other:?}"),
    }
}

fn pty_lsp() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());
    let mut reader = BufReader::new(input);
    let caller_uri = std::env::var("LATTELENS_TEST_CALLER_URI").unwrap();
    let target_uri = std::env::var("LATTELENS_TEST_TARGET_URI").unwrap();
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    let release = PathBuf::from(std::env::var_os("LATTELENS_TEST_RELEASE").unwrap());
    append_trace(&trace, &format!("helper-started={}", std::process::id()));

    let initialize = read_lsp_message(&mut reader);
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "result":{"capabilities":{
                "definitionProvider":true,
                "referencesProvider":true,
                "implementationProvider":true,
                "documentSymbolProvider":true,
                "textDocumentSync":{"openClose":true,"change":0}
            }}
        }),
    );
    let mut definition_count = 0;
    let mut reference_count = 0;
    let mut implementation_count = 0;
    let mut document_symbol_count = 0;
    let mut pending_server_calls = HashSet::new();
    let server_calls = pty_server_calls(&caller_uri);
    let mut next_server_call = 0;
    let mut server_notification_sent = false;
    loop {
        let message = read_lsp_message(&mut reader);
        match message["method"].as_str() {
            Some("initialized") => {
                send_next_pty_server_call(
                    &mut writer,
                    &server_calls,
                    &mut next_server_call,
                    &mut pending_server_calls,
                    &mut server_notification_sent,
                );
                append_trace(&trace, "initialized-received");
            }
            Some("textDocument/didOpen") => append_trace(&trace, "did-open"),
            Some("textDocument/didClose") => append_trace(&trace, "did-close"),
            Some("$/cancelRequest") => append_trace(&trace, "cancel-received"),
            Some("textDocument/definition") => {
                definition_count += 1;
                append_trace(&trace, &format!("definition-{definition_count}"));
                if definition_count == 2 {
                    append_trace(&trace, "definition-held");
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !release.exists() {
                        assert!(
                            Instant::now() < deadline,
                            "PTY definition release timed out"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                let result = match definition_count {
                    4 => serde_json::Value::Null,
                    5 => serde_json::json!({
                        "targetUri":target_uri,
                        "targetRange":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":17}
                        },
                        "targetSelectionRange":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":11}
                        },
                        "originSelectionRange":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":6}
                        }
                    }),
                    6 => serde_json::json!({
                        "uri":"file:///tmp/latte-lens-outside.rs",
                        "range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":1}
                        }
                    }),
                    7 => serde_json::json!({
                        "uri":target_uri,
                        "range":{
                            "start":{"line":999,"character":0},
                            "end":{"line":999,"character":1}
                        }
                    }),
                    8 => serde_json::json!([
                        {"uri":target_uri,"range":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":11}
                        }},
                        {"uri":caller_uri,"range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":6}
                        }},
                        {"uri":target_uri,"range":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":11}
                        }}
                    ]),
                    9 => serde_json::json!(42),
                    _ => serde_json::json!({
                        "uri":target_uri,"range":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":11}
                        }
                    }),
                };
                write_lsp_message(
                    &mut writer,
                    &serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"], "result":result
                    }),
                );
            }
            Some("textDocument/references") => {
                reference_count += 1;
                append_trace(&trace, &format!("references-{reference_count}"));
                let result = match reference_count {
                    2 => serde_json::json!([]),
                    3 => serde_json::json!([{"uri":target_uri,"range":{
                        "start":{"line":0,"character":7},
                        "end":{"line":0,"character":11}
                    }}]),
                    4 => serde_json::json!([
                        {"uri":caller_uri,"range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":6}
                        }},
                        {"targetUri":target_uri,"targetRange":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":17}
                        },"targetSelectionRange":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":11}
                        }}
                    ]),
                    _ => serde_json::json!([
                        {"uri":caller_uri,"range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":6}
                        }},
                        {"uri":target_uri,"range":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":11}
                        }}
                    ]),
                };
                write_lsp_message(
                    &mut writer,
                    &serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"], "result":result
                    }),
                );
            }
            Some("textDocument/implementation") => {
                implementation_count += 1;
                append_trace(&trace, &format!("implementation-{implementation_count}"));
                let response = match implementation_count {
                    1 => serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"],
                        "error":{"code":-32001,"message":"implementation fixture error"}
                    }),
                    2 => serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"], "result":null
                    }),
                    4 => serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"], "result":[
                            {"uri":caller_uri,"range":{
                                "start":{"line":0,"character":0},
                                "end":{"line":0,"character":6}
                            }},
                            {"uri":target_uri,"range":{
                                "start":{"line":0,"character":7},
                                "end":{"line":0,"character":11}
                            }}
                        ]
                    }),
                    _ => serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"],
                        "result":{"uri":target_uri,"range":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":11}
                        }}
                    }),
                };
                write_lsp_message(&mut writer, &response);
            }
            Some("textDocument/documentSymbol") => {
                document_symbol_count += 1;
                append_trace(&trace, &format!("document-symbol-{document_symbol_count}"));
                let result = match document_symbol_count {
                    1 => serde_json::json!([{
                        "name":"LargeRoot",
                        "detail":"nested fixture",
                        "kind":12,
                        "range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":49}
                        },
                        "selectionRange":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":17}
                        },
                        "children":[{
                            "name":"NestedSymbol",
                            "detail":"child fixture",
                            "kind":12,
                            "range":{
                                "start":{"line":0,"character":23},
                                "end":{"line":0,"character":49}
                            },
                            "selectionRange":{
                                "start":{"line":0,"character":30},
                                "end":{"line":0,"character":43}
                            }
                        }]
                    }]),
                    2 => serde_json::json!([{
                        "name":"FlatSymbol",
                        "kind":12,
                        "location":{
                            "uri":caller_uri,
                            "range":{
                                "start":{"line":0,"character":56},
                                "end":{"line":0,"character":67}
                            }
                        },
                        "containerName":"fixture"
                    }]),
                    3 => serde_json::Value::Null,
                    4 => serde_json::json!([{
                        "name":"ChildrenNull",
                        "kind":12,
                        "range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":20}
                        },
                        "selectionRange":{
                            "start":{"line":0,"character":7},
                            "end":{"line":0,"character":17}
                        },
                        "children":null
                    }]),
                    5 => serde_json::json!([]),
                    6 => serde_json::json!([
                        {
                            "name":"NestedVariant", "kind":12,
                            "range":{"start":{"line":0,"character":0},
                                "end":{"line":0,"character":20}},
                            "selectionRange":{"start":{"line":0,"character":7},
                                "end":{"line":0,"character":17}}
                        },
                        {
                            "name":"FlatVariant", "kind":12,
                            "location":{"uri":caller_uri,"range":{
                                "start":{"line":0,"character":7},
                                "end":{"line":0,"character":17}
                            }}
                        }
                    ]),
                    _ => serde_json::json!([{
                        "name":"OutsideSelection", "kind":12,
                        "range":{"start":{"line":0,"character":0},
                            "end":{"line":0,"character":5}},
                        "selectionRange":{"start":{"line":0,"character":7},
                            "end":{"line":0,"character":17}}
                    }]),
                };
                write_lsp_message(
                    &mut writer,
                    &serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"], "result":result
                    }),
                );
            }
            Some("shutdown") => {
                write_lsp_message(
                    &mut writer,
                    &serde_json::json!({"jsonrpc":"2.0","id":message["id"],"result":null}),
                );
            }
            Some("exit") => {
                append_trace(&trace, "orderly-exit");
                return;
            }
            None => {
                validate_pty_server_call_response(&message, &trace, &mut pending_server_calls);
                send_next_pty_server_call(
                    &mut writer,
                    &server_calls,
                    &mut next_server_call,
                    &mut pending_server_calls,
                    &mut server_notification_sent,
                );
            }
            other => panic!("unexpected PTY LSP method: {other:?} {message}"),
        }
    }
}

fn pty_server_calls(caller_uri: &str) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "jsonrpc":"2.0", "id":"config-valid", "method":"workspace/configuration",
            "params":{"items":[{"scopeUri":caller_uri,"section":"rust"},{}]}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"config-invalid", "method":"workspace/configuration",
            "params":{"items":"invalid"}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"folders-valid", "method":"workspace/workspaceFolders",
            "params":null
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"folders-absent", "method":"workspace/workspaceFolders"
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"folders-invalid", "method":"workspace/workspaceFolders",
            "params":{}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"apply-edit", "method":"workspace/applyEdit",
            "params":{"label":"read-only check","edit":{"changes":{}}}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"register", "method":"client/registerCapability",
            "params":{"registrations":[{"id":"one","method":"workspace/didChangeConfiguration"}]}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"unregister", "method":"client/unregisterCapability",
            "params":{"unregisterations":[{"id":"one","method":"workspace/didChangeConfiguration"}]}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"progress", "method":"window/workDoneProgress/create",
            "params":{"token":"progress-one"}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"progress-negative", "method":"window/workDoneProgress/create",
            "params":{"token":-7}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"progress-positive", "method":"window/workDoneProgress/create",
            "params":{"token":7}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"message", "method":"window/showMessageRequest",
            "params":{"type":3,"message":"bounded fixture message","actions":[{"title":"OK"}]}
        }),
        serde_json::json!({
            "jsonrpc":"2.0", "id":"unknown", "method":"fixture/unknown", "params":null
        }),
    ]
}

fn send_next_pty_server_call<W: Write>(
    writer: &mut W,
    calls: &[serde_json::Value],
    next: &mut usize,
    pending: &mut HashSet<String>,
    notification_sent: &mut bool,
) {
    if let Some(call) = calls.get(*next) {
        pending.insert(call["id"].as_str().unwrap().to_owned());
        write_lsp_message(writer, call);
        *next += 1;
        return;
    }
    if !*notification_sent {
        write_lsp_message(
            writer,
            &serde_json::json!({
                "jsonrpc":"2.0", "method":"window/logMessage",
                "params":{"type":3,"message":"notification without response"}
            }),
        );
        *notification_sent = true;
    }
}

fn validate_pty_server_call_response(
    message: &serde_json::Value,
    trace: &Path,
    pending: &mut HashSet<String>,
) {
    let id = message["id"]
        .as_str()
        .unwrap_or_else(|| panic!("unexpected response id: {message}"));
    assert!(
        pending.remove(id),
        "unexpected server-call response: {message}"
    );
    match id {
        "config-valid" => assert_eq!(message["result"], serde_json::json!([null, null])),
        "folders-valid" | "folders-absent" => {
            assert_eq!(message["result"][0]["name"], "workspace");
            assert!(
                message["result"][0]["uri"]
                    .as_str()
                    .unwrap()
                    .starts_with("file:")
            );
        }
        "apply-edit" => {
            assert_eq!(message["result"]["applied"], false);
            assert_eq!(
                message["result"]["failureReason"],
                "Latte Lens is read-only"
            );
        }
        "register" | "unregister" | "progress" | "progress-negative" | "progress-positive"
        | "message" => {
            assert!(message["result"].is_null());
        }
        "config-invalid" | "folders-invalid" => {
            assert_eq!(message["error"]["code"], -32602);
        }
        "unknown" => assert_eq!(message["error"]["code"], -32601),
        _ => unreachable!(),
    }
    append_trace(trace, &format!("server-call-ok={id}"));
}

fn framed_lsp() {
    let root_uri = std::env::var("LATTELENS_TEST_ROOT_URI").unwrap();
    let caller_uri = std::env::var("LATTELENS_TEST_CALLER_URI").unwrap();
    let target_uri = std::env::var("LATTELENS_TEST_TARGET_URI").unwrap();
    let caller_text = std::env::var("LATTELENS_TEST_CALLER_TEXT").unwrap();
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    let release = PathBuf::from(std::env::var_os("LATTELENS_TEST_RELEASE").unwrap());
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());
    let mut reader = BufReader::new(input);
    append_trace(&trace, "helper-started");

    let initialize = read_lsp_message(&mut reader);
    append_trace(&trace, &format!("initialize-read={initialize}"));
    assert_eq!(initialize["jsonrpc"], "2.0");
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(initialize["params"]["rootUri"], root_uri);
    assert_eq!(initialize["params"]["workspaceFolders"][0]["uri"], root_uri);
    assert_eq!(
        initialize["params"]["capabilities"]["general"]["positionEncodings"][0],
        "utf-16"
    );
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0",
            "id":initialize["id"],
            "result":{"capabilities":{
                "definitionProvider":true,
                "referencesProvider":true,
                "implementationProvider":true,
                "textDocumentSync":{"openClose":true,"change":0}
            }}
        }),
    );
    let initialized = read_lsp_message(&mut reader);
    assert_eq!(initialized["method"], "initialized");
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":"config-valid", "method":"workspace/configuration",
            "params":{"items":[
                {"scopeUri":caller_uri,"section":"rust"},
                {"section":"editor"},
                {}
            ]}
        }),
    );

    let mut did_open_seen = false;
    let mut valid_configuration_seen = false;
    let mut definition = None;
    while !did_open_seen || !valid_configuration_seen || definition.is_none() {
        let message = read_lsp_message(&mut reader);
        match message["method"].as_str() {
            Some("textDocument/didOpen") => {
                assert_eq!(message["params"]["textDocument"]["uri"], caller_uri);
                assert_eq!(message["params"]["textDocument"]["languageId"], "rust");
                assert_eq!(message["params"]["textDocument"]["version"], 1);
                assert_eq!(
                    message["params"]["textDocument"]["text"],
                    caller_text.trim_end()
                );
                did_open_seen = true;
            }
            Some("textDocument/definition") => {
                assert_eq!(message["params"]["textDocument"]["uri"], caller_uri);
                assert_eq!(message["params"]["position"]["line"], 0);
                assert_eq!(message["params"]["position"]["character"], 0);
                definition = Some(message);
            }
            None if message["id"] == "config-valid" => {
                assert_eq!(message["result"], serde_json::json!([null, null, null]));
                valid_configuration_seen = true;
            }
            other => panic!("unexpected framed LSP message: {other:?} {message}"),
        }
    }
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":"config-invalid", "method":"workspace/configuration",
            "params":{"items":"invalid"}
        }),
    );
    let invalid_configuration = read_lsp_message(&mut reader);
    assert_eq!(invalid_configuration["id"], "config-invalid");
    assert_eq!(invalid_configuration["error"]["code"], -32602);
    let definition = definition.unwrap();
    append_trace(&trace, "configuration-validated");
    append_trace(&trace, "definition-received");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !release.exists() {
        assert!(Instant::now() < deadline, "definition release timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":definition["id"],
            "result":{"uri":target_uri,"range":{
                "start":{"line":0,"character":7},
                "end":{"line":0,"character":11}
            }}
        }),
    );

    let shutdown = read_lsp_message(&mut reader);
    assert_eq!(shutdown["method"], "shutdown");
    write_lsp_message(
        &mut writer,
        &serde_json::json!({"jsonrpc":"2.0","id":shutdown["id"],"result":null}),
    );
    let exit = read_lsp_message(&mut reader);
    assert_eq!(exit["method"], "exit");
    append_trace(&trace, "orderly-exit");
}

fn session_reuse_lsp() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());
    let mut reader = BufReader::new(input);
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());

    let initialize = read_lsp_message(&mut reader);
    assert_eq!(initialize["method"], "initialize");
    let root_uri = initialize["params"]["rootUri"].as_str().unwrap().to_owned();
    append_trace(&trace, &format!("session-started={root_uri}"));
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "result":{"capabilities":{
                "definitionProvider":true,
                "textDocumentSync":{"openClose":true,"change":0}
            }}
        }),
    );

    loop {
        let message = read_lsp_message(&mut reader);
        match message["method"].as_str() {
            Some(
                "initialized"
                | "textDocument/didOpen"
                | "textDocument/didClose"
                | "$/cancelRequest",
            ) => {}
            Some("textDocument/definition") => {
                append_trace(&trace, &format!("definition={root_uri}"));
                write_lsp_message(
                    &mut writer,
                    &serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"], "result":null
                    }),
                );
            }
            Some("shutdown") => write_lsp_message(
                &mut writer,
                &serde_json::json!({
                    "jsonrpc":"2.0", "id":message["id"], "result":null
                }),
            ),
            Some("exit") => {
                append_trace(&trace, &format!("session-exited={root_uri}"));
                return;
            }
            other => panic!("unexpected session-reuse LSP method: {other:?} {message}"),
        }
    }
}

fn stalled_session_tree() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());
    let mut reader = BufReader::new(input);
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());

    let initialize = read_lsp_message(&mut reader);
    assert_eq!(initialize["method"], "initialize");
    let root_uri = initialize["params"]["rootUri"].as_str().unwrap().to_owned();
    append_trace(&trace, &format!("stalled-root={root_uri}"));
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "result":{"capabilities":{
                "definitionProvider":true,
                "textDocumentSync":{"openClose":true,"change":0}
            }}
        }),
    );
    let executable = std::env::current_exe().unwrap();
    let mut descendant = Command::new(executable)
        .arg("pipe-holder")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    append_trace(&trace, &format!("stalled-descendant={}", descendant.id()));
    append_trace(&trace, &format!("stalled-direct={}", std::process::id()));

    loop {
        let message = read_lsp_message(&mut reader);
        match message["method"].as_str() {
            Some("initialized" | "textDocument/didOpen" | "textDocument/didClose") => {}
            Some("textDocument/definition") => {
                append_trace(&trace, &format!("definition={root_uri}"));
                write_lsp_message(
                    &mut writer,
                    &serde_json::json!({
                        "jsonrpc":"2.0", "id":message["id"], "result":null
                    }),
                );
            }
            Some("shutdown") => {
                append_trace(&trace, &format!("stalled-shutdown={root_uri}"));
                std::thread::sleep(Duration::from_secs(30));
                // The production test terminates this process group first. If
                // the helper unexpectedly survives, explicitly reap its child.
                let _ = descendant.kill();
                let _ = descendant.wait();
                return;
            }
            Some("$/cancelRequest") => {}
            other => panic!("unexpected stalled-session LSP method: {other:?} {message}"),
        }
    }
}

fn descendant() {
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    let executable = std::env::current_exe().unwrap();
    let child = Command::new(executable)
        .arg("pipe-holder")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    append_trace(&trace, &format!("descendant={}", child.id()));
    append_trace(&trace, &format!("direct={}", std::process::id()));
    std::process::exit(17);
}

fn ready_descendant() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());
    let mut reader = BufReader::new(input);
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    let initialize = read_lsp_message(&mut reader);
    assert_eq!(initialize["method"], "initialize");
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "result":{"capabilities":{
                "definitionProvider":true,
                "textDocumentSync":{"openClose":true,"change":1}
            }}
        }),
    );
    let initialized = read_lsp_message(&mut reader);
    assert_eq!(initialized["method"], "initialized");

    let executable = std::env::current_exe().unwrap();
    let child = Command::new(executable)
        .arg("pipe-holder")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    append_trace(&trace, "ready-before-direct-exit");
    append_trace(&trace, &format!("descendant={}", child.id()));
    append_trace(&trace, &format!("direct={}", std::process::id()));
    std::process::exit(17);
}

fn incompatible_initialize() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());
    let mut reader = BufReader::new(input);
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    let initialize = read_lsp_message(&mut reader);
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "result":{"capabilities":{"positionEncoding":"utf-8"}}
        }),
    );
    append_trace(&trace, "utf8-initialize-sent");
    std::thread::sleep(Duration::from_secs(30));
}

fn crash_initialize() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    append_trace(&trace, "crash-started");
    let mut reader = BufReader::new(input);
    let initialize = read_lsp_message(&mut reader);
    assert_eq!(initialize["method"], "initialize");
    append_trace(&trace, "crash-after-initialize");
    std::process::exit(19);
}

fn timeout_navigation() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    let mut reader = BufReader::new(input);
    let initialize = read_lsp_message(&mut reader);
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "result":{"capabilities":{
                "definitionProvider":true,
                "textDocumentSync":{"openClose":true,"change":0}
            }}
        }),
    );
    loop {
        let message = read_lsp_message(&mut reader);
        match message["method"].as_str() {
            Some("initialized" | "textDocument/didOpen") => {}
            Some("textDocument/definition") => {
                append_trace(&trace, "timeout-definition-held");
                std::thread::sleep(Duration::from_secs(30));
            }
            other => panic!("unexpected timeout LSP message: {other:?} {message}"),
        }
    }
}

fn pty_resilience() {
    #[cfg(unix)]
    let input: Box<dyn Read> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdin fd.
        Box::new(unsafe { fs::File::from_raw_fd(0) })
    };
    #[cfg(unix)]
    let mut writer: Box<dyn Write> = {
        use std::os::fd::FromRawFd;
        // SAFETY: this helper process uniquely owns its inherited stdout fd.
        Box::new(unsafe { fs::File::from_raw_fd(1) })
    };
    #[cfg(windows)]
    let input: Box<dyn Read> = Box::new(std::io::stdin());
    #[cfg(windows)]
    let mut writer: Box<dyn Write> = Box::new(std::io::stdout());

    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    let count_path = PathBuf::from(std::env::var_os("LATTELENS_TEST_LAUNCH_COUNT").unwrap());
    let launch = fs::read_to_string(&count_path)
        .ok()
        .and_then(|value| value.trim().parse::<u8>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    fs::write(&count_path, launch.to_string()).unwrap();
    append_trace(
        &trace,
        &format!("resilience-launch={launch} pid={}", std::process::id()),
    );

    #[cfg(unix)]
    if launch == 4 {
        // SAFETY: this test-only process deliberately verifies the production
        // process-group cleanup escalation from SIGTERM to SIGKILL.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        append_trace(
            &trace,
            &format!("resilience-ignore-term-pid={}", std::process::id()),
        );
    }

    let mut reader = BufReader::new(input);
    let initialize = read_lsp_message(&mut reader);
    assert_eq!(initialize["method"], "initialize");
    if launch == 2 {
        append_trace(&trace, "resilience-initialize-denied");
        write_lsp_message(
            &mut writer,
            &serde_json::json!({
                "jsonrpc":"2.0", "id":initialize["id"],
                "error":{"code":-32002,"message":"fixture initialize denied"}
            }),
        );
        std::thread::sleep(Duration::from_secs(30));
        return;
    }

    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "result":{"capabilities":{
                "definitionProvider":true,
                "referencesProvider":launch != 1,
                "textDocumentSync":{"openClose":true,"change":0}
            }}
        }),
    );
    append_trace(&trace, &format!("resilience-initialize-ok={launch}"));

    loop {
        let message = read_lsp_message(&mut reader);
        match message["method"].as_str() {
            Some("initialized" | "textDocument/didOpen") => {}
            Some("textDocument/references") => {
                panic!("launch {launch} unexpectedly received an unsupported References request")
            }
            Some("textDocument/definition") => {
                append_trace(&trace, &format!("resilience-definition={launch}"));
                // Every response below deliberately makes production retire
                // this helper. Persist the armed receipt before the write so
                // process cleanup cannot win a post-send trace race.
                append_trace(&trace, &format!("resilience-response={launch}"));
                match launch {
                    1 => write_lsp_message(
                        &mut writer,
                        &serde_json::json!({
                            "jsonrpc":"2.0", "id":message["id"],
                            "error":{"code":-32003,"message":["invalid error shape"]}
                        }),
                    ),
                    3 => {
                        let malformed = vec![b'['; 129];
                        write_raw_lsp_body(&mut writer, &malformed);
                    }
                    4 => write_lsp_message(
                        &mut writer,
                        &serde_json::json!({"jsonrpc":"2.0","result":null}),
                    ),
                    5 => write_lsp_message(
                        &mut writer,
                        &serde_json::json!({
                            "jsonrpc":"2.0", "id":message["id"],
                            "result":null,
                            "error":{"code":-32004,"message":"both response fields"}
                        }),
                    ),
                    other => panic!("unexpected resilience launch: {other}"),
                }
                std::thread::sleep(Duration::from_secs(30));
                return;
            }
            other => panic!("unexpected resilience LSP method: {other:?} {message}"),
        }
    }
}

fn write_raw_lsp_body<W: Write>(writer: &mut W, body: &[u8]) {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    writer.write_all(body).unwrap();
    writer.flush().unwrap();
}

#[cfg(unix)]
#[allow(clippy::zombie_processes)] // deliberately orphaned; integration test kills the traced pid
fn escaped_pipe_owner() {
    use std::os::{fd::FromRawFd, unix::process::CommandExt};

    // SAFETY: this helper process uniquely owns its inherited stdio fds.
    let input: Box<dyn Read> = Box::new(unsafe { fs::File::from_raw_fd(0) });
    // SAFETY: this helper process uniquely owns its inherited stdio fds.
    let mut writer: Box<dyn Write> = Box::new(unsafe { fs::File::from_raw_fd(1) });
    let trace = PathBuf::from(std::env::var_os("LATTELENS_TEST_TRACE").unwrap());
    append_trace(&trace, "escaped-started");
    let mut reader = BufReader::new(input);
    let initialize = read_lsp_message(&mut reader);
    assert_eq!(initialize["method"], "initialize");
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("pipe-holder")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .unwrap();
    append_trace(&trace, &format!("escaped={}", child.id()));
    append_trace(&trace, &format!("direct={}", std::process::id()));
    write_lsp_message(
        &mut writer,
        &serde_json::json!({
            "jsonrpc":"2.0", "id":initialize["id"],
            "error":{"code":-32002,"message":"escape containment for quarantine test"}
        }),
    );
    std::thread::sleep(Duration::from_secs(30));
}

fn read_lsp_message<R: BufRead + Read>(reader: &mut R) -> serde_json::Value {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "unexpected LSP EOF"
        );
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.expect("Content-Length")];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn write_lsp_message<W: Write>(writer: &mut W, value: &serde_json::Value) {
    let body = serde_json::to_vec(value).unwrap();
    write!(
        writer,
        "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
        body.len()
    )
    .unwrap();
    writer.write_all(&body).unwrap();
    writer.flush().unwrap();
}

fn append_trace(path: &Path, line: &str) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    // A single append write keeps markers from concurrently exiting LSP
    // processes from being interleaved at the byte level.
    file.write_all(format!("{line}\n").as_bytes()).unwrap();
    file.sync_all().unwrap();
}
