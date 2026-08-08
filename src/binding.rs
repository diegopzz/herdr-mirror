// Keybinding setup CLI: discovery plus explicit, echoed edits to herdr's
// config. Nothing here runs in the background — every mutation is a command
// the user typed, printed back as it happens.
//
//   herdr-mirror remote-actions [host]      # list invokable plugin actions
//   herdr-mirror bind <plugin>.<action> <key>
//   herdr-mirror unbind <plugin>.<action> | <key>
//
// `bind` appends one marked [[keys.command]] block to herdr's config.toml and
// reloads the server, so the key works when the command returns. `unbind`
// removes only blocks carrying our marker — hand-written bindings are never
// touched, even for the same action.

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::load_config;
use crate::remote::RemoteHost;
use crate::util::{err, home_dir, Env, Result};

/// The stable CLI path install.sh links; used verbatim in written bindings so
/// they don't depend on the login-sh PATH (see the README's Remote plugin
/// keys section).
const CLI_PATH: &str = "~/.local/bin/herdr-mirror";

const MARKER: &str = "# herdr-mirror bind:";

fn herdr_config_path() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("herdr/config.toml"),
        _ => home_dir().join(".config/herdr/config.toml"),
    }
}

fn binding_block(spec: &str, key: &str) -> String {
    format!(
        "\n{MARKER} {spec}\n[[keys.command]]\nkey = \"{key}\"\ntype = \"shell\"\ncommand = \"{CLI_PATH} remote-invoke {spec}\"\n"
    )
}

/// plugin.action.list against one socket, flattened to (spec, title) rows.
async fn list_actions(api: &crate::api::ApiClient) -> Result<Vec<(String, String)>> {
    let res = api.request("plugin.action.list", json!({})).await?;
    let actions = res
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| err("plugin.action.list: no actions array in response"))?;
    Ok(actions
        .iter()
        .filter_map(|a| {
            let plugin = a.get("plugin_id").and_then(Value::as_str)?;
            let action = a.get("action_id").and_then(Value::as_str)?;
            let title = a.get("title").and_then(Value::as_str).unwrap_or("");
            Some((format!("{plugin}.{action}"), title.to_string()))
        })
        .collect())
}

fn print_actions(rows: &[(String, String)]) {
    let width = rows.iter().map(|(s, _)| s.len()).max().unwrap_or(0);
    for (spec, title) in rows {
        println!("  {spec:width$}  {title}");
    }
}

/// `remote-actions [host]`: what `remote-invoke` can reach. No argument lists
/// the local herdr and every configured host; a host name narrows to that
/// host ("local" is accepted for the local herdr). Unreachable hosts report
/// and don't abort the rest of the listing.
pub async fn remote_actions(env: Env, host_arg: Option<&str>) -> Result<()> {
    let show_local = matches!(host_arg, None | Some("local"));
    if show_local {
        println!("local:");
        let api = crate::api::ApiClient::connect(&env.local_socket).await?;
        print_actions(&list_actions(&api).await?);
    }

    if host_arg != Some("local") {
        // remote-actions is a discovery command, so unlike remote-invoke a
        // missing/broken hosts.toml only matters when hosts were asked for
        let hosts = match load_config(&env.config_search) {
            Ok(c) => c.hosts,
            Err(e) if host_arg.is_none() => {
                println!("(no hosts listed: {e})");
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        if let Some(name) = host_arg {
            if !hosts.iter().any(|h| h.name == name) {
                let known: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
                return Err(err(format!(
                    "unknown host {name:?} (configured: {})",
                    if known.is_empty() { "none".into() } else { known.join(", ") }
                )));
            }
        }
        for host in &hosts {
            if host_arg.is_some_and(|name| name != host.name) {
                continue;
            }
            println!("{}:", host.name);
            let mut remote = RemoteHost::new(host, &env.state_dir);
            match remote.connect_api().await {
                Ok((api, _)) => match list_actions(&api).await {
                    Ok(rows) => print_actions(&rows),
                    Err(e) => println!("  (cannot list: {e})"),
                },
                Err(e) => println!("  (unreachable: {e})"),
            }
        }
    }

    println!();
    println!("bind one (writes the keybinding and reloads herdr):");
    println!("  herdr-mirror bind <plugin>.<action> <key>");
    println!("or paste into {}:", herdr_config_path().display());
    print!("{}", binding_block("<plugin>.<action>", "prefix+alt+..."));
    Ok(())
}

/// `bind <plugin>.<action> <key>`: append the marked binding block to herdr's
/// config and reload the server. Refuses a key that's already bound anywhere
/// in the file (a duplicate key would be ambiguous, and this tool only ever
/// appends); a spec bound by us already is a no-op.
pub async fn bind(env: Env, spec: &str, key: &str) -> Result<()> {
    if matches!(spec.split_once('.'), Some((p, a)) if p.is_empty() || a.is_empty()) {
        return Err(err(format!("bad action spec {spec:?}: want <plugin>.<action>")));
    }

    let path = herdr_config_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("note: {} does not exist; creating it", path.display());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            String::new()
        }
        Err(e) => return Err(err(format!("cannot read {}: {e}", path.display()))),
    };

    if content.contains(&format!("{MARKER} {spec}\n")) {
        println!("{spec} is already bound by herdr-mirror in {}", path.display());
        return Ok(());
    }
    if content.contains(&format!("key = \"{key}\"")) {
        return Err(err(format!(
            "{key} is already bound in {}; pick another key or edit the file",
            path.display()
        )));
    }

    // reload needs the socket anyway, so connect before writing anything
    let api = crate::api::ApiClient::connect(&env.local_socket).await?;
    match list_actions(&api).await {
        Ok(rows) if !rows.iter().any(|(s, _)| s == spec) => {
            println!("note: {spec} is not on the local herdr; assuming it exists on the remote");
        }
        _ => {}
    }

    let block = binding_block(spec, key);
    fs::write(&path, format!("{content}{block}"))
        .map_err(|e| err(format!("cannot write {}: {e}", path.display())))?;
    println!("appended to {}:{block}", path.display());

    api.request("server.reload_config", json!({})).await?;
    println!("reloaded herdr config; {key} is live");
    Ok(())
}

/// `unbind <plugin>.<action> | <key>`: remove the marked block matching the
/// spec (marker line) or the key (its `key = "..."` line), plus the blank
/// line the append added. Only marker-carrying blocks are candidates.
pub async fn unbind(env: Env, what: &str) -> Result<()> {
    let path = herdr_config_path();
    let content = fs::read_to_string(&path)
        .map_err(|e| err(format!("cannot read {}: {e}", path.display())))?;

    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut removed = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(spec) = line.trim().strip_prefix(MARKER) {
            // our blocks are exactly marker + 4 lines (see binding_block)
            let block = &lines[i..(i + 5).min(lines.len())];
            let key_hit =
                block.iter().any(|l| l.trim() == format!("key = \"{what}\""));
            if spec.trim() == what || key_hit {
                if out.ends_with("\n\n") {
                    out.pop(); // the blank separator the append added
                }
                print!("removing from {}:\n{}", path.display(), block.concat());
                i += block.len();
                removed += 1;
                continue;
            }
        }
        out.push_str(line);
        i += 1;
    }

    if removed == 0 {
        return Err(err(format!(
            "no herdr-mirror-managed binding for {what:?} in {} (only blocks written by `herdr-mirror bind` are removable)",
            path.display()
        )));
    }
    fs::write(&path, out).map_err(|e| err(format!("cannot write {}: {e}", path.display())))?;

    let api = crate::api::ApiClient::connect(&env.local_socket).await?;
    api.request("server.reload_config", json!({})).await?;
    println!("reloaded herdr config");
    Ok(())
}
