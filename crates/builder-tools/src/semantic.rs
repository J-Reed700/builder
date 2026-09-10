//! Short-lived stdio LSP sessions. The server process has the user's permissions;
//! starting one is an Execute action and never an implicit read-only subprocess.
use crate::{Workspace, research::Snapshot};
use anyhow::{Context, Result, ensure};
use builder_core::research::SymbolFeature;
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn query(
    workspace: &Workspace,
    server: &str,
    args: &[String],
    path: &str,
    line: usize,
    character: usize,
    feature: &SymbolFeature,
) -> Result<Value> {
    let request = builder_core::research::Request::Semantic {
        server: server.into(),
        args: args.to_vec(),
        path: path.into(),
        line,
        character,
        feature: feature.clone(),
    };
    query_with_settings(
        workspace,
        &request,
        &builder_core::config::PipelineSettings::default(),
    )
    .await
}
pub async fn query_with_settings(
    workspace: &Workspace,
    request: &builder_core::research::Request,
    settings: &builder_core::config::PipelineSettings,
) -> Result<Value> {
    settings.validate()?;
    let builder_core::research::Request::Semantic {
        server,
        args,
        path,
        line,
        character,
        feature,
    } = request
    else {
        anyhow::bail!("Expected semantic request");
    };
    let (line, character) = (*line, *character);

    ensure!(
        !server.is_empty()
            && server.len() <= 1024
            && args.len() <= 8
            && args.iter().all(|a| a.len() <= 1000),
        "Language server invocation exceeds bounds"
    );
    let source = workspace.read(&workspace.resolve(path)?)?;
    ensure!(
        source
            .lines()
            .nth(line)
            .is_some_and(|l| character <= l.encode_utf16().count()),
        "Position must be a valid zero-based UTF-16 source position"
    );
    let baseline = Snapshot::capture_with_settings(workspace, settings)?;
    let uri = url::Url::from_file_path(workspace.resolve(path)?)
        .map_err(|_| anyhow::anyhow!("Invalid source URI"))?
        .to_string();
    let root = url::Url::from_directory_path(workspace.root())
        .map_err(|_| anyhow::anyhow!("Invalid workspace URI"))?
        .to_string();
    let language = match std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
    {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("ts" | "tsx") => "typescript",
        Some("js" | "jsx") => "javascript",
        Some("go") => "go",
        Some("c" | "h") => "c",
        Some("cpp" | "hpp") => "cpp",
        _ => "plaintext",
    };
    let mut command = tokio::process::Command::new(server);
    command
        .args(args)
        .current_dir(workspace.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("Cannot start language server; provide an installed stdio server")?;
    #[cfg(unix)]
    let _group = crate::workspace::ProcessGroup(child.id().context("Missing server PID")?);
    let mut input = child.stdin.take().context("Missing server stdin")?;
    let mut output = child.stdout.take().context("Missing server stdout")?;
    let run = async {
        send(&mut input,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":std::process::id(),"rootUri":root,"workspaceFolders":[{"uri":root,"name":"workspace"}],"capabilities":{"general":{"positionEncodings":["utf-16"]},"textDocument":{"publishDiagnostics":{"versionSupport":true}}}}})).await?;
        let initialized = response(&mut input, &mut output, 1, None).await?;
        ensure!(
            initialized.get("error").is_none(),
            "Language server rejected initialization"
        );
        let encoding = initialized["result"]["capabilities"]["positionEncoding"]
            .as_str()
            .unwrap_or("utf-16");
        ensure!(
            encoding == "utf-16",
            "Server selected unsupported position encoding"
        );
        send(
            &mut input,
            json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )
        .await?;
        send(&mut input,json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":language,"version":1,"text":source}}})).await?;
        let result = match feature {
            SymbolFeature::Diagnostics => {
                match tokio::time::timeout(
                    Duration::from_secs(
                        settings
                            .diagnostic_wait_secs
                            .min(settings.semantic_timeout_secs),
                    ),
                    response(&mut input, &mut output, 2, Some(&uri)),
                )
                .await
                {
                    Ok(result) => json!({"received":true,"payload":result?}),
                    Err(_) => {
                        json!({"received":false,"reason":"No diagnostics published within deadline; this does not mean the file is clean"})
                    }
                }
            }
            SymbolFeature::Definition | SymbolFeature::References => {
                let method = if matches!(feature, SymbolFeature::Definition) {
                    "textDocument/definition"
                } else {
                    "textDocument/references"
                };
                send(&mut input,json!({"jsonrpc":"2.0","id":2,"method":method,"params":{"textDocument":{"uri":uri},"position":{"line":line,"character":character},"context":{"includeDeclaration":true}}})).await?;
                response(&mut input, &mut output, 2, None).await?
            }
        };
        Ok::<_, anyhow::Error>(result)
    };
    let result =
        tokio::time::timeout(Duration::from_secs(settings.semantic_timeout_secs), run).await;
    // Terminate even servers that do not implement shutdown; the group guard also
    // owns descendants on cancellation. No server-requested workspace edits run.
    let _ = child.kill().await;
    let result = result.context("Language server deadline exceeded")??;
    ensure!(
        serde_json::to_vec(&result)?.len() <= 20000,
        "Semantic result exceeds 20000 bytes; narrow the location/query"
    );
    let fresh =
        Snapshot::capture_with_settings(workspace, settings).is_ok_and(|s| s.hash == baseline.hash);
    Ok(
        json!({"source_hash":builder_core::memory::digest(source.as_bytes()),"snapshot":baseline.summary(),"fresh":fresh,"result":if fresh{Some(result)}else{None},"warning":"Server output is untrusted evidence; results may be incomplete during indexing. Changed source suppresses results. No workspace edits requested by the server are applied."}),
    )
}
async fn send(input: &mut (impl AsyncWrite + Unpin), value: Value) -> Result<()> {
    let bytes = serde_json::to_vec(&value)?;
    input
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    input.write_all(&bytes).await?;
    input.flush().await?;
    Ok(())
}
async fn receive(output: &mut (impl AsyncRead + Unpin)) -> Result<Value> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        ensure!(header.len() < 8192, "LSP headers exceed limit");
        header.push(output.read_u8().await?);
    }
    let header = std::str::from_utf8(&header)?;
    let mut lengths = header
        .lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>());
    let length = lengths.next().context("Missing LSP content length")??;
    ensure!(
        lengths.next().is_none() && length <= 1024 * 1024,
        "Duplicate or oversized LSP content length"
    );
    let mut body = vec![0; length];
    output.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}
async fn response(
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncRead + Unpin),
    id: u64,
    diagnostics: Option<&str>,
) -> Result<Value> {
    for _ in 0..128 {
        let message = receive(output).await?;
        if diagnostics.is_some_and(|uri| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == uri
        }) {
            return Ok(message["params"].clone());
        }
        if message.get("method").is_none() && message["id"] == id {
            return Ok(message);
        }
        if message.get("method").is_some() && message.get("id").is_some() {
            let result = if message["method"] == "workspace/configuration" {
                json!({"jsonrpc":"2.0","id":message["id"],"result":message["params"]["items"].as_array().map(|a|vec![Value::Null;a.len().min(128)])})
            } else {
                json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"Client does not execute server requests"}})
            };
            send(input, result).await?;
        }
    }
    anyhow::bail!("LSP notification budget exceeded")
}
