use std::process::{Command, Stdio};

pub fn enabled() -> bool {
    std::env::var("CB_NO_NOTIFY")
        .ok()
        .is_none_or(|value| value.trim().is_empty())
}

pub fn send(title: &str, body: &str) {
    if !enabled() {
        return;
    }
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("osascript");
        command.args([
            "-e",
            &format!(
                "display notification {} with title {}",
                applescript_string(body),
                applescript_string(title)
            ),
        ]);
        command
    } else if cfg!(target_os = "linux") {
        let mut command = Command::new("notify-send");
        command.args([title, body]);
        command
    } else {
        return;
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = command.spawn();
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_literals_are_escaped_without_a_shell() {
        assert_eq!(applescript_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(applescript_string("back\\slash"), "\"back\\\\slash\"");
    }
}
