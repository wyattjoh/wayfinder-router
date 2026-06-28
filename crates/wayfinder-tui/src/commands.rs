//! Slash-command parsing for the interactive chat, ported from the Python TUI's
//! `parse_command` and `_HELP` (`wayfinder_router/tui.py`).
//!
//! The parse is the pure half of the command surface: it splits a composer line into a
//! command name plus its argument (or a plain prompt). The dispatch that drives state +
//! renderers + workers lives on [`crate::app::App`], which owns the transcript and the
//! worker plumbing the commands act on.

/// The routing scopes `/scope` accepts (mirrors the Python `_SCOPES`).
pub const SCOPES: [&str; 4] = ["turn", "last_user", "user", "all"];

/// The `/help` text (ported verbatim from the Python `_HELP`, em dashes already absent).
pub const HELP: &str = "\
commands
  /init [hybrid|openai|gemini]  scaffold a wayfinder-router.toml and load its models
  /models                       show configured models and whether each key is set
  /keys                         re-check keys: resolve from your secret store, fix hints
  /cost                         session routing mix and estimated savings vs cloud
  /new                          start a fresh conversation (the current one is saved)
  /threads      /open <n>       list saved conversations · reopen one
  /route <model>|auto           pin every turn to a model (the router still shows why)
  /local        /cloud          pin to the cheapest / most-capable tier; /auto clears
  /local <msg>  /cloud <msg>    force just this turn (kept in the thread)
  /btw <question>               quick one-off aside → local, not added to the thread
  /threshold <0..1>              set the local/cloud cut
  /scope turn|last_user|user|all what each turn scores
  /sticky on|off [N]            keep hard chats on cloud (cooldown N)
  /why [on|off|N]               expand the last (or Nth) decision; on/off auto-expands
  /stream on|off                stream replies token-by-token
  /theme dark|light|auto        recolour
  /settings                     show current settings
  /help    /quit
keys: ↑↓ history · tab expand the last why · esc or ctrl-c cancel a reply
anything else is routed.";

/// Split a composer line. `/cmd arg` -> `(Some(cmd), arg)`; plain text -> `(None, text)`.
///
/// Mirrors the Python `parse_command`: the command name is lower-cased, the argument is
/// everything after the first run of whitespace (leading whitespace trimmed, inner spaces
/// preserved). A bare `/` parses to an empty command, which the dispatch treats as unknown.
pub fn parse_command(line: &str) -> (Option<String>, String) {
    if !line.starts_with('/') {
        return (None, line.to_owned());
    }
    let trimmed = line[1..].trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("").to_lowercase();
    let arg = parts.next().unwrap_or("").trim_start().to_owned();
    (Some(cmd), arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_a_prompt() {
        assert_eq!(
            parse_command("Explain DNS in one sentence."),
            (None, "Explain DNS in one sentence.".to_owned())
        );
    }

    #[test]
    fn slash_splits_command_and_argument() {
        assert_eq!(
            parse_command("/route prefer-hosted"),
            (Some("route".to_owned()), "prefer-hosted".to_owned())
        );
        // The name lower-cases; inner spaces in the argument survive, the leading run does not.
        assert_eq!(
            parse_command("/ROUTE   gpt 4o"),
            (Some("route".to_owned()), "gpt 4o".to_owned())
        );
        // No argument.
        assert_eq!(
            parse_command("/auto"),
            (Some("auto".to_owned()), String::new())
        );
        // A bare slash is an empty command.
        assert_eq!(parse_command("/"), (Some(String::new()), String::new()));
    }
}
