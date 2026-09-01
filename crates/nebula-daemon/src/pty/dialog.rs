//! Startup dialogs an agent CLI opens *instead of* its input box.
//!
//! A task run types its prompt at a CLI it cannot see. When the CLI is
//! showing a modal instead of a prompt box, the paste lands on the modal and
//! the `\r` submit answers it — with whatever option the cursor happens to be
//! on. Claude Code has two of these, and they are not the same dialog:
//!
//! - the **trust prompt**, opened in a directory it has not seen before. It
//!   is stateful: accepting it writes `hasTrustDialogAccepted` into
//!   `~/.claude.json`, so it is asked once per project.
//! - the **bypass-permissions warning**, opened by
//!   `--dangerously-skip-permissions` — which is exactly the flag an
//!   unattended run launches with. Nothing records an acceptance for it (the
//!   per-project keys in `~/.claude.json` have no bypass-shaped entry), so it
//!   is asked on *every* launch, even in a checkout the CLI knows well:
//!
//! ```text
//! WARNING: Claude Code running in Bypass Permissions mode
//! ...
//! ❯ 1. No, exit
//!   2. Yes, I accept
//!   Enter to confirm · Esc to cancel
//! ```
//!
//! The cursor starts on "No, exit", so a prompt submitted into it exits the
//! CLI with code 1 before a single turn runs — which is what every unattended
//! run did until this module existed.
//!
//! Detection reads the PTY stream, not a screen: the CLI writes each word at
//! an absolute column (`ESC [ 12 G`) rather than separating them with spaces,
//! so the markers below are matched against text with **every escape sequence
//! and every space removed**. That normalisation also makes a line wrap
//! invisible, which is why a marker may safely span one.

/// Fragments that together identify the bypass-permissions warning, spaces
/// already removed (see the module note on why the stream has none).
/// Both must appear: the first alone would also match the CLI merely
/// *printing* the words, and this decides whether to press keys.
const BYPASS_MARKERS: [&str; 2] = ["InBypassPermissionsmode,", "Yes,Iaccept"];

/// Fragments identifying the trust prompt — *any* one of them is enough,
/// because the wording has moved between CLI versions and this is only ever
/// used to name a failure. nebula does not answer this one: accepting a
/// directory on the user's behalf is a decision about *their* filesystem,
/// and unlike the bypass warning it is remembered, so answering it once
/// silently would grant trust they never gave.
const TRUST_MARKERS: [&str; 2] = [
    "Doyoutrustthefilesinthisfolder",
    "Isthisaprojectyoucreatedoroneyoutrust",
];

/// Keys that accept the bypass warning: ↓ to step off "No, exit" onto
/// "Yes, I accept", then Enter to confirm. It is a two-option list and the
/// cursor always starts on the refusal, so this is deterministic — but it is
/// also why [`visible`] is re-checked afterwards rather than assumed.
pub const ACCEPT_BYPASS_KEYS: &[u8] = b"\x1b[B\r";

/// How much of the tail to look at. The dialog is the whole screen when it
/// is up, and bounding the scan keeps a long scrollback from being walked on
/// every delivery.
const SCAN_BYTES: usize = 16 * 1024;

/// A modal standing between the CLI and its input box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupDialog {
    /// `--dangerously-skip-permissions` wants an explicit "yes, I accept".
    BypassPermissions,
    /// A checkout the CLI has not been run in before.
    Trust,
}

impl StartupDialog {
    /// How to describe it in a run outcome, in the sentence
    /// "the CLI is waiting at …".
    pub fn describe(&self) -> &'static str {
        match self {
            StartupDialog::BypassPermissions => "its bypass-permissions warning",
            StartupDialog::Trust => "its trust prompt for this checkout",
        }
    }
}

/// The dialog showing at the end of `output`, if any.
pub fn visible(output: &[u8]) -> Option<StartupDialog> {
    let tail = &output[output.len().saturating_sub(SCAN_BYTES)..];
    let text = normalize(tail);
    if BYPASS_MARKERS.iter().all(|m| text.contains(m)) {
        return Some(StartupDialog::BypassPermissions);
    }
    if TRUST_MARKERS.iter().any(|m| text.contains(m)) {
        return Some(StartupDialog::Trust);
    }
    None
}

/// Strip escape sequences and whitespace, leaving only the characters the
/// dialog's words are made of.
fn normalize(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != 0x1b {
            if !b.is_ascii_whitespace() {
                out.push(b);
            }
            i += 1;
            continue;
        }
        i += 1;
        match bytes.get(i) {
            // CSI: parameters, then a final byte in @..~.
            Some(b'[') => {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            }
            // OSC and the other string sequences: run to BEL or ESC \.
            Some(b']') | Some(b'P') | Some(b'X') | Some(b'^') | Some(b'_') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // Charset selection takes one more byte; every other two-byte
            // escape (ESC 7, ESC 8, …) is consumed by the `i += 1` alone.
            Some(b'(') | Some(b')') | Some(b'*') | Some(b'+') => i += 2,
            Some(_) => i += 1,
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real thing, byte for byte, from the transcript of a run that died
    /// on it (`nebula runs show --transcript`, task `heartbeat`, 2026-08-31).
    /// Note there is not one space in it: every word is placed with `ESC[nG`.
    const BYPASS_SCREEN: &[u8] = b"\x1b7\x1b[r\x1b8\x1b[?25h\x1b[?2004h\r\r\n\
\x1b[38;5;211m\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\x1b[39m\r\r\n\
\x1b[3G\x1b[38;5;211m\x1b[1mWARNING:\x1b[12GClaude\x1b[19GCode\x1b[24Grunning\x1b[32Gin\x1b[35GBypass\x1b[42GPermissions\x1b[54Gmode\x1b[22m\x1b[39m\r\r\n\
\x1b[3GIn\x1b[6GBypass\x1b[13GPermissions\x1b[25Gmode,\x1b[31GClaude\x1b[38GCode\x1b[43Gwill\x1b[48Gnot\x1b[52Gask\x1b[56Gfor\x1b[60Gyour\x1b[65Gapproval\r\r\n\
\x1b[3Gbefore\x1b[10Grunning\x1b[18Gpotentially\x1b[30Gdangerous\x1b[40Gcommands.\r\r\n\
\x1b[3G\x1b]8;id=zaxmda;https://code.claude.com/docs/en/security\x07https://code.claude.com/docs/en/security\x1b]8;;\x07\r\r\n\
\x1b[3G\x1b[38;5;153m\xe2\x9d\xaf\x1b[5G\x1b[38;5;246m1.\x1b[8G\x1b[38;5;153mNo,\x1b[12Gexit\x1b[39m\r\r\n\
\x1b[5G\x1b[38;5;246m2.\x1b[8G\x1b[39mYes,\x1b[13GI\x1b[15Gaccept\r\r\n\
\x1b[3G\x1b[38;5;246m\x1b[3mEnter\x1b[9Gto\x1b[12Gconfirm\x1b[20G\xc2\xb7\x1b[22GEsc\x1b[26Gto\x1b[29Gcancel\x1b[23m\x1b[39m\r\r\n";

    #[test]
    fn the_bypass_warning_is_recognised_in_the_raw_stream() {
        assert_eq!(
            visible(BYPASS_SCREEN),
            Some(StartupDialog::BypassPermissions)
        );
    }

    /// The words arrive spaced rather than column-positioned when the CLI is
    /// not driving a real terminal — the markers must not depend on which.
    #[test]
    fn spacing_and_wrapping_do_not_change_the_verdict() {
        let plain = b"WARNING: Claude Code running in Bypass Permissions mode\n\
                      In Bypass Permissions mode, Claude Code will not ask\n\
                      1. No, exit\n  2. Yes, I accept\n";
        assert_eq!(visible(plain), Some(StartupDialog::BypassPermissions));

        // The same text wrapped mid-marker: the newline vanishes with every
        // other space, so the marker still matches.
        let wrapped = b"In Bypass Permissions mode, Claude Code\nwill not ask\n2. Yes, I\naccept\n";
        assert_eq!(visible(wrapped), Some(StartupDialog::BypassPermissions));
    }

    /// One marker is not enough: the CLI writing *about* bypass mode, or a
    /// prompt that quotes this module, must not get keys pressed at it.
    #[test]
    fn half_a_match_presses_nothing() {
        let mention = b"I ran it in Bypass Permissions mode, as you asked.\n";
        assert_eq!(visible(mention), None);
        let answer_only = b"2. Yes, I accept\n";
        assert_eq!(visible(answer_only), None);
    }

    #[test]
    fn an_ordinary_session_is_not_a_dialog() {
        assert_eq!(visible(b""), None);
        assert_eq!(
            visible(b"\x1b[2J\x1b[H> \xe2\x95\xad tell me what to do \xe2\x95\xae"),
            None
        );
    }

    #[test]
    fn the_trust_prompt_is_named_but_not_answered() {
        let trust = b"Do you trust the files in this folder?\n\
                      /Users/t/dev/x\n\
                      \xe2\x9d\xaf 1. Yes, proceed\n  2. No, exit\n\
                      Enter to confirm \xc2\xb7 Esc to exit\n";
        assert_eq!(visible(trust), Some(StartupDialog::Trust));
        assert_eq!(
            StartupDialog::Trust.describe(),
            "its trust prompt for this checkout"
        );
    }

    /// Only the tail is scanned, so a dialog that has since been answered and
    /// scrolled away by a long session does not read as still up.
    #[test]
    fn only_the_tail_counts() {
        let mut stream = BYPASS_SCREEN.to_vec();
        stream.extend_from_slice(&vec![b'x'; SCAN_BYTES]);
        assert_eq!(visible(&stream), None);
    }

    /// ↓ then Enter — the second option, confirmed.
    #[test]
    fn the_accept_keys_are_down_then_enter() {
        assert_eq!(ACCEPT_BYPASS_KEYS, b"\x1b[B\r");
    }
}
