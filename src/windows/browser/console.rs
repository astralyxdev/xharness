//! A Browser window's console: what the page logs (`console.log`, `.warn`,
//! `.error`…) and its uncaught errors, shown in a drawer under the page.
//!
//! A script WebKit runs in every page before the page's own (see
//! [`SCRIPT`]) wraps the console and listens for errors, and posts each one
//! to the app as JSON (`{"level": "warn", "text": "…"}`).

use std::collections::VecDeque;

use serde::Deserialize;

/// How many messages the drawer keeps (the oldest go first).
const LIMIT: usize = 500;
/// Longest message kept, in characters (the script cuts them too).
const MAX_TEXT: usize = 4000;

/// The name the page posts to (`window.webkit.messageHandlers.<name>`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const HANDLER: &str = "xharnessConsole";

/// Runs at document start in every page: forwards console calls and
/// uncaught errors to the app, then lets them through as usual.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const SCRIPT: &str = r#"(function () {
  if (window.__xharnessConsole) return;
  window.__xharnessConsole = true;
  var handler = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.xharnessConsole;
  if (!handler) return;
  function show(value) {
    if (typeof value === 'string') return value;
    if (value instanceof Error) {
      // WebKit's stacks leave the message out: "Error: bad", then where.
      var head = String(value);
      if (!value.stack) return head;
      return value.stack.indexOf(head) === 0 ? value.stack : head + '\n' + value.stack;
    }
    try {
      var json = JSON.stringify(value);
      return json === undefined ? String(value) : json;
    } catch (e) {
      return String(value);
    }
  }
  function post(level, values) {
    try {
      var text = Array.prototype.map.call(values, show).join(' ');
      handler.postMessage(JSON.stringify({ level: level, text: text.slice(0, 4000) }));
    } catch (e) {}
  }
  ['log', 'info', 'warn', 'error', 'debug'].forEach(function (level) {
    var original = console[level];
    console[level] = function () {
      post(level, arguments);
      return original.apply(console, arguments);
    };
  });
  window.addEventListener('error', function (event) {
    if (event.error) {
      post('error', ['Uncaught ' + show(event.error)]);
      return;
    }
    // Another origin's script: the browser hides what it was.
    var where = event.filename ? ' (' + event.filename + ':' + event.lineno + ')' : '';
    post('error', [(event.message || 'Error') + where]);
  });
  window.addEventListener('unhandledrejection', function (event) {
    post('error', ['Unhandled rejection: ' + show(event.reason)]);
  });
})();"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Log,
    Info,
    Warn,
    Error,
    Debug,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub level: Level,
    pub text: String,
}

/// One message as the page posts it. `None` for anything else.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse(json: &str) -> Option<Message> {
    #[derive(Deserialize)]
    struct Posted {
        level: Level,
        text: String,
    }
    let posted: Posted = serde_json::from_str(json).ok()?;
    let text = match posted.text.char_indices().nth(MAX_TEXT) {
        Some((cut, _)) => posted.text[..cut].to_string(),
        None => posted.text,
    };
    Some(Message { level: posted.level, text })
}

/// The messages since the page loaded.
#[derive(Default)]
pub struct Console {
    messages: VecDeque<Message>,
    errors: usize,
}

impl Console {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn push(&mut self, message: Message) {
        if self.messages.len() == LIMIT
            && let Some(old) = self.messages.pop_front()
            && old.level == Level::Error
        {
            self.errors -= 1;
        }
        if message.level == Level::Error {
            self.errors += 1;
        }
        self.messages.push_back(message);
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.errors = 0;
    }

    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn errors(&self) -> usize {
        self.errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posted_messages_are_read_and_anything_else_is_not() {
        assert_eq!(
            parse(r#"{"level":"warn","text":"careful"}"#),
            Some(Message { level: Level::Warn, text: "careful".into() })
        );
        assert_eq!(parse(r#"{"level":"shout","text":"x"}"#), None);
        assert_eq!(parse("not json"), None);
        let long = format!(r#"{{"level":"log","text":"{}"}}"#, "é".repeat(MAX_TEXT + 10));
        assert_eq!(parse(&long).unwrap().text.chars().count(), MAX_TEXT);
    }

    #[test]
    fn it_keeps_the_latest_and_counts_errors() {
        let mut console = Console::default();
        console.push(Message { level: Level::Error, text: "first".into() });
        for ix in 0..LIMIT {
            console.push(Message { level: Level::Log, text: ix.to_string() });
        }
        assert_eq!(console.len(), LIMIT);
        // The first error scrolled out.
        assert_eq!(console.errors(), 0);
        console.push(Message { level: Level::Error, text: "again".into() });
        assert_eq!(console.errors(), 1);
        console.clear();
        assert_eq!((console.len(), console.errors()), (0, 0));
    }

    #[test]
    fn the_script_posts_to_the_handler_by_its_name() {
        assert!(SCRIPT.contains(&format!("messageHandlers.{HANDLER}")));
    }
}
