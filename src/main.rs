use niri_ipc::socket::SOCKET_PATH_ENV;
use niri_ipc::{Event, Request, Response, Window, Workspace};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

fn main() {
    loop {
        if let Err(e) = run_daemon() {
            eprintln!("Daemon error: {}, reconnecting in 1s...", e);
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    // Subscribe to event stream. Niri sends the full current workspace/window
    // state up-front, then incremental updates. Keeping that state locally
    // avoids opening fresh IPC sockets on every focus/workspace event, which
    // adds visible latency to Waybar previews/hover updates.
    let mut stream = connect_to_niri()?;
    send_request(&mut stream, &Request::EventStream)?;

    let mut reader = BufReader::new(stream);
    let reply: Result<Response, String> = read_json_line(&mut reader)?;

    match reply {
        Ok(Response::Handled) => {}
        Ok(other) => return Err(format!("Unexpected response: {:?}", other).into()),
        Err(msg) => return Err(msg.into()),
    }

    let _ = reader.get_mut().shutdown(Shutdown::Write);
    let mut workspaces = Vec::new();
    let mut windows = Vec::new();
    let mut have_workspaces = false;
    let mut have_windows = false;

    loop {
        let Some(event) = read_next_known_event(&mut reader)? else {
            continue;
        };

        let mut should_output = true;
        match event {
            Event::WorkspacesChanged {
                workspaces: new_workspaces,
            } => {
                workspaces = new_workspaces;
                have_workspaces = true;
            }
            Event::WorkspaceActivated { id, focused } => {
                let output = workspaces
                    .iter()
                    .find(|ws| ws.id == id)
                    .and_then(|ws| ws.output.clone());

                for ws in &mut workspaces {
                    if ws.id == id {
                        ws.is_active = true;
                        ws.is_focused = focused;
                    } else {
                        if ws.output == output {
                            ws.is_active = false;
                        }
                        if focused {
                            ws.is_focused = false;
                        }
                    }
                }
            }
            Event::WorkspaceActiveWindowChanged {
                workspace_id,
                active_window_id,
            } => {
                if let Some(ws) = workspaces.iter_mut().find(|ws| ws.id == workspace_id) {
                    ws.active_window_id = active_window_id;
                }
            }
            Event::WorkspaceUrgencyChanged { id, urgent } => {
                if let Some(ws) = workspaces.iter_mut().find(|ws| ws.id == id) {
                    ws.is_urgent = urgent;
                }
            }
            Event::WindowsChanged {
                windows: new_windows,
            } => {
                windows = new_windows;
                have_windows = true;
            }
            Event::WindowOpenedOrChanged { window } => {
                if window.is_focused {
                    for win in &mut windows {
                        win.is_focused = false;
                    }
                }

                if let Some(existing) = windows.iter_mut().find(|win| win.id == window.id) {
                    *existing = window;
                } else {
                    windows.push(window);
                }
            }
            Event::WindowClosed { id } => {
                windows.retain(|win| win.id != id);
                for ws in &mut workspaces {
                    if ws.active_window_id == Some(id) {
                        ws.active_window_id = None;
                    }
                }
            }
            Event::WindowFocusChanged { id } => {
                for win in &mut windows {
                    win.is_focused = Some(win.id) == id;
                }
            }
            Event::WindowLayoutsChanged { changes } => {
                for (id, layout) in changes {
                    if let Some(win) = windows.iter_mut().find(|win| win.id == id) {
                        win.layout = layout;
                    }
                }
            }
            _ => should_output = false,
        }

        if should_output && have_workspaces && have_windows {
            output_status(&workspaces, &windows);
        }
    }
}

fn connect_to_niri() -> Result<UnixStream, Box<dyn std::error::Error>> {
    let socket_path = env::var_os(SOCKET_PATH_ENV).ok_or_else(|| {
        format!("{SOCKET_PATH_ENV} is not set, are you running this within niri?")
    })?;
    Ok(UnixStream::connect(socket_path)?)
}

fn send_request(
    stream: &mut UnixStream,
    request: &Request,
) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut *stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_json_line<T: serde::de::DeserializeOwned>(
    reader: &mut BufReader<UnixStream>,
) -> Result<T, Box<dyn std::error::Error>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err("niri IPC socket closed".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn read_next_known_event(
    reader: &mut BufReader<UnixStream>,
) -> Result<Option<Event>, Box<dyn std::error::Error>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err("niri IPC event stream closed".into());
    }

    match serde_json::from_str(&line) {
        Ok(event) => Ok(Some(event)),
        Err(_err) => Ok(None),
    }
}

fn output_status(workspaces: &[Workspace], windows: &[Window]) {
    // Pre-compute terminal apps.
    // Avoid per-pid process detection for foot/footclient when multiple windows
    // share the same pid, to prevent false positives across windows.
    let mut pid_counts: HashMap<i32, usize> = HashMap::new();
    for w in windows {
        if let Some(pid) = w.pid {
            *pid_counts.entry(pid).or_insert(0) += 1;
        }
    }

    let terminal_apps: HashMap<i32, &str> = windows
        .iter()
        .filter(|w| is_terminal(w.app_id.as_deref().unwrap_or("")))
        .filter_map(|w| {
            let pid = w.pid?;
            let app_id = w.app_id.as_deref().unwrap_or("");
            if (app_id == "foot" || app_id == "footclient" || app_id == "kitty")
                && pid_counts.get(&pid).copied().unwrap_or(0) > 1
            {
                return None;
            }
            Some((pid, detect_terminal_app(pid as u32)))
        })
        .filter(|(_, app)| !app.is_empty())
        .collect();

    let mut sorted_workspaces: Vec<&Workspace> = workspaces.iter().collect();
    sorted_workspaces.sort_by_key(|w| w.idx);

    let mut before_focused: Vec<String> = Vec::new();
    let mut after_focused: Vec<String> = Vec::new();
    let mut focused_output = String::new();
    let mut current_section = "before";
    let mut tooltip = String::new();

    for ws in &sorted_workspaces {
        let mut ws_windows: Vec<&Window> = windows
            .iter()
            .filter(|w| w.workspace_id == Some(ws.id))
            .collect();

        ws_windows.sort_by_key(|win| window_sort_key(*win));

        let win_count = ws_windows.len();

        // Get workspace name if it's not default/empty
        let ws_name = ws.name.as_deref().unwrap_or("");
        let has_custom_name = !ws_name.is_empty();

        // Only add to tooltip if workspace has windows
        if win_count > 0 {
            if has_custom_name {
                tooltip.push_str(&format!("{}: {} windows\\n", ws_name, win_count));
            } else {
                tooltip.push_str(&format!("ws{}: {} windows\\n", ws.idx, win_count));
            }
        }

        // Skip empty unfocused workspaces entirely
        if win_count == 0 && !ws.is_focused {
            continue;
        }

        let ws_output = if win_count == 0 {
            // Empty but focused - just show a dot, no name
            "<span color='#888888'>·</span>".to_string()
        } else {
            let mut output = String::new();

            // Show workspace name only when focused
            if has_custom_name && ws.is_focused {
                output.push_str(&format!("<span color='#cccccc'>{}</span> ", ws_name));
            }

            for win in &ws_windows {
                let app_id = win.app_id.as_deref().unwrap_or("");
                let title = win.title.as_deref().unwrap_or("");
                let mut color = get_color(app_id, title, win.pid, &terminal_apps);
                let is_tmux = is_tmux_title(title);

                if !ws.is_focused {
                    color = dim_color(&color);
                }

                let mut bar = if win.is_focused {
                    "█"
                } else if !ws.is_focused && ws.active_window_id == Some(win.id) {
                    "▌"
                } else {
                    "|"
                };

                // Use a broken bar for tmux without changing focus semantics.
                if is_tmux && bar == "|" {
                    bar = "¦";
                }

                output.push_str(&format!("<span color='{}'>{}</span>", color, bar));
            }
            output
        };

        if ws.is_focused {
            focused_output = ws_output;
            current_section = "after";
        } else if current_section == "before" {
            before_focused.push(ws_output);
        } else {
            after_focused.push(ws_output);
        }
    }

    let mut output = String::new();
    for ws in &before_focused {
        output.push_str(ws);
        output.push_str("  ");
    }
    output.push_str(&focused_output);
    for ws in &after_focused {
        output.push_str("  ");
        output.push_str(ws);
    }

    if tooltip.ends_with("\\n") {
        tooltip.truncate(tooltip.len() - 2);
    }

    println!(r#"{{"text": "{}", "tooltip": "{}"}}"#, output, tooltip);
}

fn is_terminal(app_id: &str) -> bool {
    app_id == "Alacritty"
        || app_id == "kitty"
        || app_id == "com.mitchellh.ghostty"
        || app_id == "foot"
        || app_id == "footclient"
}

fn is_tmux_title(title: &str) -> bool {
    title.to_lowercase().contains("tmux")
}

fn is_claude_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    lower.contains("claude") || title.contains("✳")
}

fn detect_terminal_app(pid: u32) -> &'static str {
    let mut found_claude = false;
    let mut found_codex = false;
    let mut found_jcode = false;
    let mut found_nvim = false;

    if let Ok(children) = get_all_descendants(pid) {
        for child_pid in children {
            if let Ok(cmdline) = fs::read_to_string(format!("/proc/{}/cmdline", child_pid)) {
                let cmdline = cmdline.to_lowercase();
                if cmdline.contains("jcode") {
                    found_jcode = true;
                }
                if cmdline.contains("claude") {
                    found_claude = true;
                }
                if cmdline.contains("codex") {
                    found_codex = true;
                }
                if cmdline.contains("nvim") || cmdline.contains("vim") {
                    found_nvim = true;
                }
            }
        }
    }

    if found_jcode {
        "jcode"
    } else if found_claude {
        "claude"
    } else if found_codex {
        "codex"
    } else if found_nvim {
        "nvim"
    } else {
        ""
    }
}

fn get_all_descendants(pid: u32) -> std::io::Result<Vec<u32>> {
    let mut descendants = Vec::new();
    let mut to_check = vec![pid];

    while let Some(current) = to_check.pop() {
        let children_path = format!("/proc/{}/task/{}/children", current, current);
        if let Ok(children_str) = fs::read_to_string(&children_path) {
            for child in children_str.split_whitespace() {
                if let Ok(child_pid) = child.parse::<u32>() {
                    descendants.push(child_pid);
                    to_check.push(child_pid);
                }
            }
        }
    }

    Ok(descendants)
}

fn get_color(
    app_id: &str,
    title: &str,
    pid: Option<i32>,
    terminal_apps: &HashMap<i32, &str>,
) -> String {
    if is_terminal(app_id) {
        let title_lower = title.to_lowercase();

        if title_lower.contains("jcode") {
            return "#999999".to_string();
        } else if is_claude_title(title) {
            return "#f5a623".to_string();
        } else if title_lower.contains("codex") {
            return "#56b6c2".to_string();
        } else if title_lower.contains("nvim") || title_lower.contains("vim") {
            return "#98c379".to_string();
        }

        if let Some(pid) = pid {
            if let Some(&app) = terminal_apps.get(&pid) {
                return match app {
                    "jcode" => "#999999",
                    "claude" => "#f5a623",
                    "codex" => "#56b6c2",
                    "nvim" => "#98c379",
                    _ => "#888888",
                }
                .to_string();
            }
        }

        return "#888888".to_string();
    }

    if app_id.contains("nvim") {
        return "#98c379".to_string();
    }
    if app_id == "google-chrome" || app_id.contains("chrome") {
        return "#ea4335".to_string();
    }
    if app_id == "firefox" {
        return "#ff7139".to_string();
    }
    if app_id == "vesktop" || app_id == "discord" {
        return "#c678dd".to_string();
    }
    if app_id == "spotify" || app_id.contains("spotify") {
        return "#1DB954".to_string();
    }
    if app_id.contains("todoist") {
        return "#e06c75".to_string();
    }
    if app_id.contains("mail.google") {
        return "#e5c07b".to_string();
    }

    "#666666".to_string()
}

fn dim_color(hex: &str) -> String {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return format!("#{}", hex);
    }

    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);

    let r = ((r as u32 * 6 / 10) + 30).min(255) as u8;
    let g = ((g as u32 * 6 / 10) + 30).min(255) as u8;
    let b = ((b as u32 * 6 / 10) + 30).min(255) as u8;

    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

fn window_sort_key(win: &Window) -> (u8, u64, u64, u64) {
    if let Some((col, row)) = win.layout.pos_in_scrolling_layout {
        (0, col as u64, row as u64, win.id)
    } else {
        // Floating/unknown layout windows go after tiled windows.
        (1, 0, 0, win.id)
    }
}
