use base64::Engine;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;

const DEFAULT_PORT: u16 = 8765;

#[derive(Debug, Clone)]
struct Config {
    base_url: String,
    session_id: Option<String>,
    managed_context: Option<String>,
    raw: bool,
    json: bool,
}

#[derive(Debug)]
struct CommandArgs {
    positional: Vec<String>,
    values: BTreeMap<String, Vec<String>>,
    bools: BTreeSet<String>,
}

pub async fn run(raw_args: Vec<String>) -> Result<(), String> {
    let (config, command) = parse_global_args(raw_args)?;
    let (config, command) = parse_output_flags(config, command);
    if command.is_empty() {
        print_help();
        return Ok(());
    }
    if matches!(command[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    let client = reqwest::Client::new();
    match command[0].as_str() {
        "status" => {
            ensure_help(&command[1..], help_status)?;
            let response =
                call_tool(&client, &config, "get_status", Value::Object(Map::new())).await?;
            print_tool_response(response, &config, None)?;
        }
        "logs" => run_logs(&client, &config, &command[1..]).await?,
        "tools" | "tool" => run_tools(&client, &config, &command[1..]).await?,
        "display" => run_display(&client, &config, &command[1..]).await?,
        "browser" | "browsers" => run_browser(&client, &config, &command[1..]).await?,
        "cu" => run_cu(&client, &config, &command[1..]).await?,
        "shared" | "shared-view" => run_shared(&client, &config, &command[1..]).await?,
        "approval" | "approvals" => run_approval(&client, &config, &command[1..]).await?,
        "input" => run_input(&client, &config, &command[1..]).await?,
        "settings" | "set" => run_settings(&client, &config, &command[1..]).await?,
        "task" => run_task(&client, &config, &command[1..]).await?,
        "controller" => run_controller(&client, &config, &command[1..]).await?,
        "context" => run_context(&client, &config, &command[1..]).await?,
        "audio" => run_audio(&client, &config, &command[1..]).await?,
        other => {
            return Err(format!(
                "unknown command '{other}'. Run `intendant ctl --help`."
            ));
        }
    }
    Ok(())
}

fn parse_global_args(mut raw: Vec<String>) -> Result<(Config, Vec<String>), String> {
    let mut base_url = std::env::var("INTENDANT_MCP_URL").unwrap_or_default();
    let mut port = std::env::var("INTENDANT_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let mut session_id = std::env::var("INTENDANT_SESSION_ID").ok();
    let mut managed_context = std::env::var("INTENDANT_MANAGED_CONTEXT").ok();
    let mut raw_output = false;
    let mut json_output = false;
    let mut command_start = 0;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--url" => {
                i += 1;
                base_url = raw
                    .get(i)
                    .cloned()
                    .ok_or_else(|| "--url requires a value".to_string())?;
            }
            "--port" => {
                i += 1;
                let value = raw
                    .get(i)
                    .ok_or_else(|| "--port requires a value".to_string())?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port value '{value}'"))?;
            }
            "--session" | "--session-id" => {
                i += 1;
                session_id = Some(
                    raw.get(i)
                        .cloned()
                        .ok_or_else(|| "--session requires a value".to_string())?,
                );
            }
            "--managed-context" => {
                i += 1;
                managed_context = Some(
                    raw.get(i)
                        .cloned()
                        .ok_or_else(|| "--managed-context requires a value".to_string())?,
                );
            }
            "--raw" => raw_output = true,
            "--json" => json_output = true,
            arg if arg.starts_with("--url=") => {
                base_url = arg.trim_start_matches("--url=").to_string();
            }
            arg if arg.starts_with("--port=") => {
                let value = arg.trim_start_matches("--port=");
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port value '{value}'"))?;
            }
            arg if arg.starts_with("--session=") => {
                session_id = Some(arg.trim_start_matches("--session=").to_string());
            }
            arg if arg.starts_with("--session-id=") => {
                session_id = Some(arg.trim_start_matches("--session-id=").to_string());
            }
            arg if arg.starts_with("--managed-context=") => {
                managed_context = Some(arg.trim_start_matches("--managed-context=").to_string());
            }
            _ => {
                command_start = i;
                break;
            }
        }
        i += 1;
        command_start = i;
    }

    let command = raw.split_off(command_start);
    let base_url = if base_url.trim().is_empty() {
        format!("http://localhost:{port}/mcp")
    } else {
        base_url
    };

    Ok((
        Config {
            base_url,
            session_id: clean_opt(session_id),
            managed_context: clean_opt(managed_context),
            raw: raw_output,
            json: json_output,
        },
        command,
    ))
}

fn parse_output_flags(mut config: Config, raw: Vec<String>) -> (Config, Vec<String>) {
    let mut command = Vec::with_capacity(raw.len());
    for arg in raw {
        match arg.as_str() {
            "--raw" => config.raw = true,
            "--json" => config.json = true,
            _ => command.push(arg),
        }
    }
    (config, command)
}

fn clean_opt(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_command_args(
    raw: &[String],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> Result<CommandArgs, String> {
    let value_flags: BTreeSet<&str> = value_flags.iter().copied().collect();
    let bool_flags: BTreeSet<&str> = bool_flags.iter().copied().collect();
    let mut positional = Vec::new();
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut bools = BTreeSet::new();
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--" {
            positional.extend(raw[i + 1..].iter().cloned());
            break;
        }
        if let Some((flag, value)) = arg.split_once('=') {
            if flag.starts_with("--") && value_flags.contains(flag) {
                values
                    .entry(flag.to_string())
                    .or_default()
                    .push(value.to_string());
            } else if flag.starts_with("--") && bool_flags.contains(flag) {
                return Err(format!("{flag} does not take a value"));
            } else {
                positional.push(arg.clone());
            }
        } else if arg.starts_with("--") && value_flags.contains(arg.as_str()) {
            i += 1;
            let value = raw
                .get(i)
                .cloned()
                .ok_or_else(|| format!("{arg} requires a value"))?;
            values.entry(arg.clone()).or_default().push(value);
        } else if arg.starts_with("--") {
            if !bool_flags.contains(arg.as_str()) {
                return Err(format!("unknown flag {arg}"));
            }
            bools.insert(arg.clone());
        } else {
            positional.push(arg.clone());
        }
        i += 1;
    }
    Ok(CommandArgs {
        positional,
        values,
        bools,
    })
}

impl CommandArgs {
    fn one(&self, flag: &str) -> Option<&str> {
        self.values
            .get(flag)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn all(&self, flag: &str) -> impl Iterator<Item = &str> {
        self.values
            .get(flag)
            .into_iter()
            .flat_map(|v| v.iter().map(String::as_str))
    }

    fn has(&self, flag: &str) -> bool {
        self.bools.contains(flag)
    }
}

async fn run_logs(client: &reqwest::Client, config: &Config, raw: &[String]) -> Result<(), String> {
    ensure_help(raw, help_logs)?;
    let args = parse_command_args(raw, &["--since-id", "--level", "--limit"], &[])?;
    let mut map = Map::new();
    insert_u64(&mut map, "since_id", args.one("--since-id"))?;
    insert_string(&mut map, "level_filter", args.one("--level"));
    insert_usize(&mut map, "limit", args.one("--limit"))?;
    let response = call_tool(client, config, "get_logs", Value::Object(map)).await?;
    print_tool_response(response, config, None)
}

async fn run_tools(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_tools();
        return Ok(());
    }
    match raw[0].as_str() {
        "list" => {
            ensure_help(&raw[1..], help_tools_list)?;
            let response = rpc(client, config, "tools/list", Value::Object(Map::new())).await?;
            if config.raw || config.json {
                print_json(&response)?;
            } else {
                let tools = response
                    .pointer("/result/tools")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "tools/list response missing result.tools".to_string())?;
                for tool in tools {
                    let name = tool
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<unnamed>");
                    let description = tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .replace('\n', " ");
                    if description.is_empty() {
                        println!("{name}");
                    } else {
                        println!("{name}\t{description}");
                    }
                }
            }
        }
        "schema" | "help" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let name = args
                .positional
                .first()
                .ok_or_else(|| "tools schema requires a tool name".to_string())?;
            let response = rpc(client, config, "tools/list", Value::Object(Map::new())).await?;
            let tools = response
                .pointer("/result/tools")
                .and_then(Value::as_array)
                .ok_or_else(|| "tools/list response missing result.tools".to_string())?;
            let tool = tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()))
                .ok_or_else(|| format!("tool '{name}' is not advertised by this MCP endpoint"))?;
            print_json(tool)?;
        }
        "call" => {
            ensure_help(&raw[1..], help_tools_call)?;
            let args = parse_command_args(&raw[1..], &["--args", "--arg"], &[])?;
            let name = args
                .positional
                .first()
                .ok_or_else(|| "tools call requires a tool name".to_string())?;
            let arguments = tool_arguments_from_flags(&args)?;
            let response = call_tool(client, config, name, arguments).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown tools command '{other}'")),
    }
    Ok(())
}

async fn run_display(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_display();
        return Ok(());
    }
    match raw[0].as_str() {
        "list" => {
            let response =
                call_tool(client, config, "list_displays", Value::Object(Map::new())).await?;
            print_tool_response(response, config, None)?;
        }
        "frames" => {
            let args = parse_command_args(&raw[1..], &["--stream", "--count"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "stream", args.one("--stream"));
            insert_usize(&mut map, "count", args.one("--count"))?;
            let response = call_tool(client, config, "list_frames", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "read-frame" | "frame" => {
            let args = parse_command_args(&raw[1..], &["--stream"], &[])?;
            let frame_id = args
                .positional
                .first()
                .cloned()
                .unwrap_or_else(|| "latest".to_string());
            let mut map = Map::new();
            map.insert("frame_id".to_string(), Value::String(frame_id));
            insert_string(&mut map, "stream", args.one("--stream"));
            let response = call_tool(client, config, "read_frame", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "screenshot" => {
            ensure_help(&raw[1..], help_display_screenshot)?;
            let args = parse_command_args(&raw[1..], &["--target", "--output"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "display_target", args.one("--target"));
            let response = call_tool(client, config, "take_screenshot", Value::Object(map)).await?;
            print_tool_response(response, config, output_path(args.one("--output")))?;
        }
        "take" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let id = positional_u32(&args, 0, "display take requires a display id")?;
            let mut map = Map::new();
            map.insert("display_id".to_string(), Value::from(id));
            let response = call_tool(client, config, "take_display", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "release" => {
            let args = parse_command_args(&raw[1..], &["--note"], &[])?;
            let id = positional_u32(&args, 0, "display release requires a display id")?;
            let mut map = Map::new();
            map.insert("display_id".to_string(), Value::from(id));
            insert_string(&mut map, "note", args.one("--note"));
            let response = call_tool(client, config, "release_display", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown display command '{other}'")),
    }
    Ok(())
}

async fn run_browser(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_browser();
        return Ok(());
    }
    match raw[0].as_str() {
        "providers" => {
            let response = call_tool(
                client,
                config,
                "browser_workspace_providers",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "list" | "ls" => {
            let response = call_tool(
                client,
                config,
                "list_browser_workspaces",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "create" | "open" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--url",
                    "--label",
                    "--provider",
                    "--peer",
                    "--session",
                    "--profile-dir",
                ],
                &[],
            )?;
            let mut map = Map::new();
            let url = args
                .one("--url")
                .or_else(|| args.positional.first().map(String::as_str));
            insert_string(&mut map, "url", url);
            insert_string(&mut map, "label", args.one("--label"));
            insert_string(&mut map, "provider", args.one("--provider"));
            insert_string(&mut map, "peer_id", args.one("--peer"));
            insert_string(&mut map, "owner_session_id", args.one("--session"));
            insert_string(&mut map, "profile_dir", args.one("--profile-dir"));
            let response = call_tool(
                client,
                config,
                "create_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "close" => {
            let args = parse_command_args(&raw[1..], &["--reason"], &[])?;
            let id = args
                .positional
                .first()
                .ok_or_else(|| "browser close requires a workspace id".to_string())?;
            let mut map = Map::new();
            map.insert("workspace_id".to_string(), Value::String(id.clone()));
            insert_string(&mut map, "reason", args.one("--reason"));
            let response = call_tool(
                client,
                config,
                "close_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "acquire" | "take" => {
            let args = parse_command_args(
                &raw[1..],
                &["--holder", "--holder-kind", "--note"],
                &["--force"],
            )?;
            let id = args
                .positional
                .first()
                .ok_or_else(|| "browser acquire requires a workspace id".to_string())?;
            let holder = args
                .one("--holder")
                .or(config.session_id.as_deref())
                .unwrap_or("intendant-ctl");
            let mut map = Map::new();
            map.insert("workspace_id".to_string(), Value::String(id.clone()));
            map.insert("holder_id".to_string(), Value::String(holder.to_string()));
            insert_string(&mut map, "holder_kind", args.one("--holder-kind"));
            insert_string(&mut map, "note", args.one("--note"));
            if args.has("--force") {
                map.insert("force".to_string(), Value::Bool(true));
            }
            let response = call_tool(
                client,
                config,
                "acquire_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "release" => {
            let args = parse_command_args(&raw[1..], &["--holder", "--note"], &[])?;
            let id = args
                .positional
                .first()
                .ok_or_else(|| "browser release requires a workspace id".to_string())?;
            let mut map = Map::new();
            map.insert("workspace_id".to_string(), Value::String(id.clone()));
            insert_string(&mut map, "holder_id", args.one("--holder"));
            insert_string(&mut map, "note", args.one("--note"));
            let response = call_tool(
                client,
                config,
                "release_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown browser command '{other}'")),
    }
    Ok(())
}

async fn run_cu(client: &reqwest::Client, config: &Config, raw: &[String]) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_cu();
        return Ok(());
    }
    match raw[0].as_str() {
        "actions" | "exec" => {
            ensure_help(&raw[1..], help_cu_actions)?;
            let args = parse_command_args(
                &raw[1..],
                &["--actions", "--target", "--coordinate-space", "--output"],
                &[],
            )?;
            let actions = args
                .one("--actions")
                .ok_or_else(|| "cu actions requires --actions JSON".to_string())
                .and_then(read_json_value)?;
            if !actions.is_array() {
                return Err("--actions must be a JSON array".to_string());
            }
            let mut map = Map::new();
            map.insert("actions".to_string(), actions);
            insert_string(&mut map, "display_target", args.one("--target"));
            insert_string(&mut map, "coordinate_space", args.one("--coordinate-space"));
            let response =
                call_tool(client, config, "execute_cu_actions", Value::Object(map)).await?;
            print_tool_response(response, config, output_path(args.one("--output")))?;
        }
        "screenshot" => {
            let next = std::iter::once("screenshot".to_string())
                .chain(raw[1..].iter().cloned())
                .collect::<Vec<_>>();
            run_display(client, config, &next).await?;
        }
        other => return Err(format!("unknown cu command '{other}'")),
    }
    Ok(())
}

async fn run_shared(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_shared();
        return Ok(());
    }
    match raw[0].as_str() {
        "show" => {
            let args = parse_command_args(
                &raw[1..],
                &["--target", "--display-id", "--reason", "--focus"],
                &[],
            )?;
            let mut map = shared_target_map(&args)?;
            insert_string(&mut map, "reason", args.one("--reason"));
            if let Some(region) = args.one("--focus") {
                map.insert("focus_region".to_string(), parse_region(region)?);
            }
            let response =
                call_tool(client, config, "show_shared_view", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "focus" => {
            ensure_help(&raw[1..], help_shared_focus)?;
            let args = parse_command_args(
                &raw[1..],
                &["--target", "--display-id", "--region", "--note"],
                &[],
            )?;
            let mut map = shared_target_map(&args)?;
            let region = args
                .one("--region")
                .or_else(|| args.positional.first().map(String::as_str))
                .ok_or_else(|| "shared focus requires --region x,y,width,height".to_string())?;
            map.insert("region".to_string(), parse_region(region)?);
            insert_string(&mut map, "note", args.one("--note"));
            let response =
                call_tool(client, config, "focus_shared_view", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "input" | "request-input" => {
            let args =
                parse_command_args(&raw[1..], &["--target", "--display-id", "--reason"], &[])?;
            let mut map = shared_target_map(&args)?;
            insert_string(&mut map, "reason", args.one("--reason"));
            let response = call_tool(
                client,
                config,
                "request_shared_view_input",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "hide" => {
            let args = parse_command_args(&raw[1..], &["--reason"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "reason", args.one("--reason"));
            let response =
                call_tool(client, config, "hide_shared_view", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "capture" => {
            let args = parse_command_args(
                &raw[1..],
                &["--target", "--display-id", "--reason", "--output"],
                &[],
            )?;
            let mut map = shared_target_map(&args)?;
            insert_string(&mut map, "reason", args.one("--reason"));
            let response = call_tool(
                client,
                config,
                "capture_shared_view_frame",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, output_path(args.one("--output")))?;
        }
        other => return Err(format!("unknown shared command '{other}'")),
    }
    Ok(())
}

async fn run_approval(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_approval();
        return Ok(());
    }
    match raw[0].as_str() {
        "pending" => {
            let response = call_tool(
                client,
                config,
                "get_pending_approval",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "approve" | "deny" | "skip" | "approve-all" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let id = positional_u64(&args, 0, "approval action requires an id")?;
            let tool = match raw[0].as_str() {
                "approve" => "approve",
                "deny" => "deny",
                "skip" => "skip",
                "approve-all" => "approve_all",
                _ => unreachable!(),
            };
            let mut map = Map::new();
            map.insert("id".to_string(), Value::from(id));
            let response = call_tool(client, config, tool, Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown approval command '{other}'")),
    }
    Ok(())
}

async fn run_input(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_input();
        return Ok(());
    }
    match raw[0].as_str() {
        "pending" => {
            let response = call_tool(
                client,
                config,
                "get_pending_input",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "respond" => {
            let args = parse_command_args(&raw[1..], &["--text"], &[])?;
            let text = args
                .one("--text")
                .map(str::to_string)
                .or_else(|| {
                    if args.positional.is_empty() {
                        None
                    } else {
                        Some(args.positional.join(" "))
                    }
                })
                .ok_or_else(|| "input respond requires text".to_string())?;
            let mut map = Map::new();
            map.insert("text".to_string(), Value::String(text));
            let response = call_tool(client, config, "respond", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown input command '{other}'")),
    }
    Ok(())
}

async fn run_settings(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_settings();
        return Ok(());
    }
    match raw[0].as_str() {
        "autonomy" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let level = args
                .positional
                .first()
                .ok_or_else(|| "settings autonomy requires a level".to_string())?;
            let response = call_tool(
                client,
                config,
                "set_autonomy",
                json_object([("level", Value::String(level.clone()))]),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "verbosity" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let level = args
                .positional
                .first()
                .ok_or_else(|| "settings verbosity requires a level".to_string())?;
            let response = call_tool(
                client,
                config,
                "set_verbosity",
                json_object([("level", Value::String(level.clone()))]),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown settings command '{other}'")),
    }
    Ok(())
}

async fn run_task(client: &reqwest::Client, config: &Config, raw: &[String]) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_task();
        return Ok(());
    }
    match raw[0].as_str() {
        "start" => {
            let args = parse_command_args(
                &raw[1..],
                &["--task", "--display-target", "--frame"],
                &["--orchestrate", "--direct"],
            )?;
            let task = args
                .one("--task")
                .map(str::to_string)
                .or_else(|| {
                    if args.positional.is_empty() {
                        None
                    } else {
                        Some(args.positional.join(" "))
                    }
                })
                .ok_or_else(|| "task start requires a task".to_string())?;
            let mut map = Map::new();
            map.insert("task".to_string(), Value::String(task));
            if args.has("--orchestrate") {
                map.insert("orchestrate".to_string(), Value::Bool(true));
            } else if args.has("--direct") {
                map.insert("orchestrate".to_string(), Value::Bool(false));
            }
            let frames: Vec<Value> = args
                .all("--frame")
                .map(|v| Value::String(v.to_string()))
                .collect();
            if !frames.is_empty() {
                map.insert("reference_frame_ids".to_string(), Value::Array(frames));
            }
            insert_string(&mut map, "display_target", args.one("--display-target"));
            let response = call_tool(client, config, "start_task", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown task command '{other}'")),
    }
    Ok(())
}

async fn run_controller(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_controller();
        return Ok(());
    }
    match raw[0].as_str() {
        "status" => {
            let response = call_tool(
                client,
                config,
                "get_controller_loop_status",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "restart-status" => {
            let response = call_tool(
                client,
                config,
                "get_restart_status",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "halt" => {
            let args = parse_command_args(&raw[1..], &[], &["--one-shot"])?;
            let mut map = Map::new();
            if args.has("--one-shot") {
                map.insert("persistent".to_string(), Value::Bool(false));
            }
            let response = call_tool(
                client,
                config,
                "request_controller_loop_halt",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "clear-halt" => {
            let response = call_tool(
                client,
                config,
                "clear_controller_loop_halt",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "intervene" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let mode = args
                .positional
                .first()
                .ok_or_else(|| "controller intervene requires stop or abort".to_string())?;
            let response = call_tool(
                client,
                config,
                "intervene_controller_loop",
                json_object([("mode", Value::String(mode.clone()))]),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "schedule" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--controller-id",
                    "--goal",
                    "--reason",
                    "--after",
                    "--command",
                    "--max-attempts",
                    "--cooldown-sec",
                ],
                &["--auto-start"],
            )?;
            let mut map = Map::new();
            insert_required_string(&mut map, "controller_id", args.one("--controller-id"))?;
            insert_required_string(&mut map, "north_star_goal", args.one("--goal"))?;
            insert_string(&mut map, "reason", args.one("--reason"));
            insert_string(&mut map, "restart_after", args.one("--after"));
            insert_string(&mut map, "restart_command", args.one("--command"));
            insert_u32(&mut map, "max_attempts", args.one("--max-attempts"))?;
            insert_u64(&mut map, "cooldown_sec", args.one("--cooldown-sec"))?;
            if args.has("--auto-start") {
                map.insert("auto_start_task".to_string(), Value::Bool(true));
            }
            let response = call_tool(
                client,
                config,
                "schedule_controller_restart",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "cancel" => {
            let args = parse_command_args(&raw[1..], &["--restart-id"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "restart_id", args.one("--restart-id"));
            let response = call_tool(
                client,
                config,
                "cancel_controller_restart",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "complete" => {
            let args = parse_command_args(
                &raw[1..],
                &["--restart-id", "--token", "--status", "--summary"],
                &[],
            )?;
            let mut map = Map::new();
            insert_required_string(&mut map, "restart_id", args.one("--restart-id"))?;
            insert_required_string(&mut map, "turn_complete_token", args.one("--token"))?;
            insert_string(&mut map, "status", args.one("--status"));
            insert_string(&mut map, "handoff_summary", args.one("--summary"));
            let response = call_tool(
                client,
                config,
                "controller_turn_complete",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown controller command '{other}'")),
    }
    Ok(())
}

async fn run_context(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_context();
        return Ok(());
    }
    match raw[0].as_str() {
        "rewind" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--session",
                    "--item-id",
                    "--proof",
                    "--position",
                    "--reason",
                    "--primer",
                    "--preserve",
                    "--discard",
                    "--artifact",
                    "--next-step",
                ],
                &[],
            )?;
            let mut map = Map::new();
            insert_string(&mut map, "session_id", args.one("--session"));
            let item_id = args
                .one("--item-id")
                .ok_or_else(|| "context rewind requires --item-id".to_string())?;
            let proof = match args.one("--proof").map(str::trim).filter(|v| !v.is_empty()) {
                Some(proof) => proof.to_string(),
                None => {
                    resolve_context_rewind_anchor_proof(
                        client,
                        config,
                        args.one("--session"),
                        item_id,
                    )
                    .await?
                }
            };
            let position = args.one("--position").unwrap_or("before");
            map.insert(
                "anchor".to_string(),
                json_object([
                    ("item_id", Value::String(item_id.to_string())),
                    ("proof", Value::String(proof)),
                    ("position", Value::String(position.to_string())),
                ]),
            );
            insert_required_string(&mut map, "reason", args.one("--reason"))?;
            insert_required_string(&mut map, "primer", args.one("--primer"))?;
            insert_string_array(&mut map, "preserve", args.all("--preserve"));
            insert_string_array(&mut map, "discard", args.all("--discard"));
            insert_string_array(&mut map, "artifacts", args.all("--artifact"));
            insert_string_array(&mut map, "next_steps", args.all("--next-step"));
            let response = call_tool(client, config, "rewind_context", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "backout" => {
            let args = parse_command_args(
                &raw[1..],
                &["--session", "--record-id", "--mode", "--name"],
                &["--allow-cache-reset"],
            )?;
            let mut map = Map::new();
            insert_string(&mut map, "session_id", args.one("--session"));
            insert_required_string(&mut map, "record_id", args.one("--record-id"))?;
            insert_string(&mut map, "mode", args.one("--mode"));
            insert_string(&mut map, "name", args.one("--name"));
            if args.has("--allow-cache-reset") {
                map.insert("allow_cache_reset".to_string(), Value::Bool(true));
            }
            let response = call_tool(client, config, "rewind_backout", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "claim-fission" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--group-id",
                    "--branch-session-id",
                    "--expected-canonical-session-id",
                ],
                &[],
            )?;
            let mut map = Map::new();
            insert_required_string(&mut map, "group_id", args.one("--group-id"))?;
            insert_required_string(
                &mut map,
                "branch_session_id",
                args.one("--branch-session-id"),
            )?;
            insert_string(
                &mut map,
                "expected_canonical_session_id",
                args.one("--expected-canonical-session-id"),
            );
            let response = call_tool(
                client,
                config,
                "claim_fission_canonical",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown context command '{other}'")),
    }
    Ok(())
}

async fn run_audio(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_audio();
        return Ok(());
    }
    match raw[0].as_str() {
        "spawn" => {
            let args = parse_command_args(&raw[1..], &["--args"], &[])?;
            let value = args
                .one("--args")
                .ok_or_else(|| "audio spawn requires --args JSON".to_string())
                .and_then(read_json_value)?;
            if !value.is_object() {
                return Err("--args must be a JSON object".to_string());
            }
            let response = call_tool(client, config, "spawn_live_audio", value).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown audio command '{other}'")),
    }
    Ok(())
}

async fn call_tool(
    client: &reqwest::Client,
    config: &Config,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    rpc(
        client,
        config,
        "tools/call",
        serde_json::json!({
            "name": name,
            "arguments": arguments,
        }),
    )
    .await
}

async fn resolve_context_rewind_anchor_proof(
    client: &reqwest::Client,
    config: &Config,
    session_id: Option<&str>,
    item_id: &str,
) -> Result<String, String> {
    let mut args = Map::new();
    insert_string(&mut args, "session_id", session_id);
    args.insert("query".to_string(), Value::String(item_id.to_string()));
    args.insert("limit".to_string(), Value::Number(10.into()));
    let response = call_tool(client, config, "list_rewind_anchors", Value::Object(args)).await?;
    let text = tool_response_text(&response)?;
    let payload = serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("invalid list_rewind_anchors JSON while resolving proof: {e}"))?;
    let anchors = payload
        .get("anchors")
        .and_then(Value::as_array)
        .ok_or_else(|| "list_rewind_anchors response missing anchors".to_string())?;
    anchors
        .iter()
        .find(|anchor| {
            anchor
                .get("item_id")
                .and_then(Value::as_str)
                .is_some_and(|candidate| candidate == item_id)
        })
        .and_then(|anchor| anchor.get("proof").and_then(Value::as_str))
        .map(str::trim)
        .filter(|proof| !proof.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "could not resolve catalog proof for anchor `{item_id}`; run list_rewind_anchors and retry with --proof"
            )
        })
}

async fn rpc(
    client: &reqwest::Client,
    config: &Config,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let url = mcp_url(config)?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON-RPC response: {e}: {text}"))
}

fn mcp_url(config: &Config) -> Result<reqwest::Url, String> {
    let mut url =
        reqwest::Url::parse(&config.base_url).map_err(|e| format!("invalid MCP URL: {e}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(session_id) = &config.session_id {
            pairs.append_pair("session_id", session_id);
        }
        if let Some(managed_context) = &config.managed_context {
            pairs.append_pair("managed_context", managed_context);
        }
    }
    Ok(url)
}

fn tool_response_text(response: &Value) -> Result<String, String> {
    if let Some(error) = response.get("error") {
        return Err(format!("MCP tool call failed: {error}"));
    }
    let result = response
        .get("result")
        .ok_or_else(|| "JSON-RPC response missing result".to_string())?;
    let text = text_contents(result).collect::<Vec<_>>().join("\n");
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(if text.trim().is_empty() {
            "tool returned isError=true".to_string()
        } else {
            text
        });
    }
    Ok(text)
}

fn print_tool_response(
    response: Value,
    config: &Config,
    output_path: Option<PathBuf>,
) -> Result<(), String> {
    if config.raw {
        return print_json(&response);
    }
    if let Some(error) = response.get("error") {
        print_json(error)?;
        return Err("MCP tool call failed".to_string());
    }
    let result = response
        .get("result")
        .ok_or_else(|| "JSON-RPC response missing result".to_string())?;
    if config.json {
        if let Some(text) = single_text_content(result) {
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                return print_json(&value);
            }
        }
        return print_json(result);
    }
    if let Some(path) = output_path {
        save_first_image(result, &path)?;
        for text in text_contents(result) {
            println!("{text}");
        }
        println!("wrote {}", path.display());
        return Ok(());
    }
    let mut printed = false;
    for text in text_contents(result) {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            print_json(&value)?;
        } else {
            println!("{text}");
        }
        printed = true;
    }
    let images = image_contents(result).count();
    if images > 0 {
        println!("[{images} image content block(s); rerun with --output PATH to save]");
        printed = true;
    }
    if !printed {
        print_json(result)?;
    }
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("tool returned isError=true".to_string());
    }
    Ok(())
}

fn single_text_content(result: &Value) -> Option<&str> {
    let mut texts = text_contents(result);
    let first = texts.next()?;
    if texts.next().is_none() {
        Some(first)
    } else {
        None
    }
}

fn text_contents(result: &Value) -> impl Iterator<Item = &str> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|content| content.iter())
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
}

fn image_contents(result: &Value) -> impl Iterator<Item = (&str, &str)> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|content| content.iter())
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|item| {
            let data = item.get("data").and_then(Value::as_str)?;
            let mime = item
                .get("mimeType")
                .or_else(|| item.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            Some((data, mime))
        })
}

fn save_first_image(result: &Value, path: &PathBuf) -> Result<(), String> {
    let (data, _mime) = image_contents(result)
        .next()
        .ok_or_else(|| "tool result did not include an image content block".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("failed to decode image data: {e}"))?;
    std::fs::write(path, bytes).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn print_json(value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    println!("{text}");
    Ok(())
}

fn tool_arguments_from_flags(args: &CommandArgs) -> Result<Value, String> {
    let mut map = match args.one("--args") {
        Some(value) => match read_json_value(value)? {
            Value::Object(map) => map,
            _ => return Err("--args must be a JSON object".to_string()),
        },
        None => Map::new(),
    };
    for pair in args.all("--arg") {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("--arg expects key=value, got '{pair}'"))?;
        map.insert(key.to_string(), parse_jsonish(value)?);
    }
    Ok(Value::Object(map))
}

fn read_json_value(input: &str) -> Result<Value, String> {
    let text = if input == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        text
    } else if let Some(path) = input.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {path}: {e}"))?
    } else {
        input.to_string()
    };
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))
}

fn parse_jsonish(value: &str) -> Result<Value, String> {
    if matches!(value, "true" | "false" | "null")
        || value.starts_with('{')
        || value.starts_with('[')
        || value.starts_with('"')
    {
        return serde_json::from_str(value)
            .map_err(|e| format!("invalid JSON value '{value}': {e}"));
    }
    if let Ok(v) = value.parse::<i64>() {
        return Ok(Value::from(v));
    }
    if let Ok(v) = value.parse::<f64>() {
        return Ok(Value::from(v));
    }
    Ok(Value::String(value.to_string()))
}

fn parse_region(value: &str) -> Result<Value, String> {
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err("region must be x,y,width,height".to_string());
    }
    let parse = |s: &str| {
        s.parse::<f64>()
            .map_err(|_| format!("invalid region coordinate '{s}'"))
    };
    Ok(json_object([
        ("x", Value::from(parse(parts[0])?)),
        ("y", Value::from(parse(parts[1])?)),
        ("width", Value::from(parse(parts[2])?)),
        ("height", Value::from(parse(parts[3])?)),
    ]))
}

fn shared_target_map(args: &CommandArgs) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    insert_string(&mut map, "display_target", args.one("--target"));
    insert_u32(&mut map, "display_id", args.one("--display-id"))?;
    Ok(map)
}

fn output_path(value: Option<&str>) -> Option<PathBuf> {
    value.map(PathBuf::from)
}

fn json_object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

fn insert_string(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn insert_required_string(
    map: &mut Map<String, Value>,
    key: &str,
    value: Option<&str>,
) -> Result<(), String> {
    let value = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("missing required --{}", key.replace('_', "-")))?;
    map.insert(key.to_string(), Value::String(value.to_string()));
    Ok(())
}

fn insert_string_array<'a>(
    map: &mut Map<String, Value>,
    key: &str,
    values: impl Iterator<Item = &'a str>,
) {
    let values: Vec<Value> = values.map(|v| Value::String(v.to_string())).collect();
    if !values.is_empty() {
        map.insert(key.to_string(), Value::Array(values));
    }
}

fn insert_u64(map: &mut Map<String, Value>, key: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        let parsed = value
            .parse::<u64>()
            .map_err(|_| format!("invalid integer for --{}", key.replace('_', "-")))?;
        map.insert(key.to_string(), Value::from(parsed));
    }
    Ok(())
}

fn insert_u32(map: &mut Map<String, Value>, key: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        let parsed = value
            .parse::<u32>()
            .map_err(|_| format!("invalid integer for --{}", key.replace('_', "-")))?;
        map.insert(key.to_string(), Value::from(parsed));
    }
    Ok(())
}

fn insert_usize(
    map: &mut Map<String, Value>,
    key: &str,
    value: Option<&str>,
) -> Result<(), String> {
    if let Some(value) = value {
        let parsed = value
            .parse::<usize>()
            .map_err(|_| format!("invalid integer for --{}", key.replace('_', "-")))?;
        map.insert(key.to_string(), Value::from(parsed));
    }
    Ok(())
}

fn positional_u64(args: &CommandArgs, index: usize, message: &str) -> Result<u64, String> {
    args.positional
        .get(index)
        .ok_or_else(|| message.to_string())?
        .parse::<u64>()
        .map_err(|_| message.to_string())
}

fn positional_u32(args: &CommandArgs, index: usize, message: &str) -> Result<u32, String> {
    args.positional
        .get(index)
        .ok_or_else(|| message.to_string())?
        .parse::<u32>()
        .map_err(|_| message.to_string())
}

fn is_help(raw: &[String]) -> bool {
    raw.len() == 1 && matches!(raw[0].as_str(), "-h" | "--help" | "help")
}

fn ensure_help(raw: &[String], help: fn()) -> Result<(), String> {
    if is_help(raw) {
        help();
        std::process::exit(0);
    }
    Ok(())
}

fn print_help() {
    println!(
        "intendant ctl controls a running Intendant daemon through its HTTP MCP endpoint.\n\
\n\
Usage: intendant ctl [global flags] <command> [args]\n\
\n\
Global flags:\n\
  --url URL                 MCP URL (default http://localhost:8765/mcp)\n\
  --port PORT               Dashboard/MCP port when --url is omitted\n\
  --session ID              Session id to bind to the MCP request\n\
  --managed-context MODE    vanilla or managed\n\
  --json                    Print parsed JSON where possible\n\
  --raw                     Print raw JSON-RPC responses\n\
\n\
Commands:\n\
  status                    Get current status\n\
  logs                      Read log entries\n\
  tools                     Lazy MCP tool discovery and generic calls\n\
  display                   Displays, frames, screenshots, display claims\n\
  browser                   Browser workspaces and leases\n\
  cu                        Computer-use actions\n\
  shared                    Shared display collaboration\n\
  approval                  Pending approvals and approval responses\n\
  input                     Pending human question and response\n\
  settings                  Autonomy and verbosity\n\
  task                      Start tasks\n\
  controller                Controller loop and restart controls\n\
  context                   Managed-context rewind/backout controls\n\
  audio                     Live-audio controls\n\
\n\
Run `intendant ctl <command> --help` for focused help."
    );
}

fn help_status() {
    println!("Usage: intendant ctl status [--json|--raw]");
}

fn help_logs() {
    println!(
        "Usage: intendant ctl logs [--since-id N] [--level LEVEL] [--limit N]\n\
Levels include info, model, agent, error, warn, subagent, debug."
    );
}

fn help_tools() {
    println!(
        "Usage:\n\
  intendant ctl tools list\n\
  intendant ctl tools schema TOOL\n\
  intendant ctl tools call TOOL [--args JSON|@file|-] [--arg key=value]\n\
\n\
Use this for lazy discovery of rare or newly-added Intendant capabilities."
    );
}

fn help_tools_list() {
    println!("Usage: intendant ctl tools list [--json|--raw]");
}

fn help_tools_call() {
    println!(
        "Usage: intendant ctl tools call TOOL [--args JSON|@file|-] [--arg key=value]\n\
Examples:\n\
  intendant ctl tools call get_status\n\
  intendant ctl tools call get_logs --arg limit=10"
    );
}

fn help_display() {
    println!(
        "Usage:\n\
  intendant ctl display list\n\
  intendant ctl display frames [--stream NAME] [--count N]\n\
  intendant ctl display read-frame [latest|ID] [--stream NAME]\n\
  intendant ctl display screenshot [--target TARGET] [--output out.png]\n\
  intendant ctl display take DISPLAY_ID\n\
  intendant ctl display release DISPLAY_ID [--note TEXT]"
    );
}

fn help_display_screenshot() {
    println!(
        "Usage: intendant ctl display screenshot [--target TARGET] [--output out.png]\n\
Targets include user_session, display_99, 99, and legacy :99."
    );
}

fn help_browser() {
    println!(
        "Usage:\n\
  intendant ctl browser providers\n\
  intendant ctl browser list\n\
  intendant ctl browser create [URL] [--label TEXT] [--provider auto|cdp|system_cdp|playwright|agent_browser] [--peer PEER_ID] [--session ID] [--profile-dir PATH]\n\
  intendant ctl browser acquire WORKSPACE_ID [--holder ID] [--holder-kind agent|human] [--note TEXT] [--force]\n\
  intendant ctl browser release WORKSPACE_ID [--holder ID] [--note TEXT]\n\
  intendant ctl browser close WORKSPACE_ID [--reason TEXT]\n\
\n\
CDP uses a managed Chromium/Chrome-for-Testing executable by default. Use --provider system_cdp, or set INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1, to opt into system Chrome/Chromium."
    );
}

fn help_cu() {
    println!(
        "Usage:\n\
  intendant ctl cu actions --actions JSON|@file|- [--target TARGET] [--coordinate-space pixel|normalized_1000] [--output out.png]\n\
  intendant ctl cu screenshot [--target TARGET] [--output out.png]"
    );
}

fn help_cu_actions() {
    println!(
        "Usage: intendant ctl cu actions --actions JSON|@file|- [--target TARGET] [--coordinate-space pixel|normalized_1000]\n\
Actions are the same tagged objects accepted by execute_cu_actions: click, double_click, type, key, scroll, move_mouse, drag, screenshot, wait."
    );
}

fn help_shared() {
    println!(
        "Usage:\n\
  intendant ctl shared show [--target TARGET|--display-id ID] [--reason TEXT] [--focus x,y,w,h]\n\
  intendant ctl shared focus --region x,y,w,h [--target TARGET|--display-id ID] [--note TEXT]\n\
  intendant ctl shared input [--target TARGET|--display-id ID] [--reason TEXT]\n\
  intendant ctl shared capture [--target TARGET|--display-id ID] [--output out.png]\n\
  intendant ctl shared hide [--reason TEXT]\n\
\n\
Regions are normalized fractions from 0.0 to 1.0."
    );
}

fn help_shared_focus() {
    println!("Usage: intendant ctl shared focus --region x,y,width,height [--note TEXT]");
}

fn help_approval() {
    println!(
        "Usage:\n\
  intendant ctl approval pending\n\
  intendant ctl approval approve ID\n\
  intendant ctl approval deny ID\n\
  intendant ctl approval skip ID\n\
  intendant ctl approval approve-all ID"
    );
}

fn help_input() {
    println!(
        "Usage:\n\
  intendant ctl input pending\n\
  intendant ctl input respond TEXT..."
    );
}

fn help_settings() {
    println!(
        "Usage:\n\
  intendant ctl settings autonomy low|medium|high|full\n\
  intendant ctl settings verbosity quiet|normal|verbose|debug"
    );
}

fn help_task() {
    println!(
        "Usage: intendant ctl task start [--task TEXT] [--orchestrate|--direct] [--display-target TARGET] [--frame ID]\n\
If --task is omitted, remaining positional text becomes the task."
    );
}

fn help_controller() {
    println!(
        "Usage:\n\
  intendant ctl controller status\n\
  intendant ctl controller restart-status\n\
  intendant ctl controller halt [--one-shot]\n\
  intendant ctl controller clear-halt\n\
  intendant ctl controller intervene stop|abort\n\
  intendant ctl controller schedule --controller-id ID --goal TEXT [--after turn_end|now]\n\
  intendant ctl controller cancel [--restart-id ID]\n\
  intendant ctl controller complete --restart-id ID --token TOKEN [--status TEXT] [--summary TEXT]"
    );
}

fn help_context() {
    println!(
        "Usage:\n\
  intendant ctl --managed-context managed context rewind --item-id ID --position before|after --reason TEXT --primer TEXT\n\
      [--proof PROOF]\n\
  intendant ctl --managed-context managed context backout --record-id ID [--mode inspect|restore|fork|backout]\n\
  intendant ctl context claim-fission --group-id ID --branch-session-id ID"
    );
}

fn help_audio() {
    println!(
        "Usage: intendant ctl audio spawn --args JSON|@file|-\n\
The JSON object is the spawn_live_audio parameter object."
    );
}
