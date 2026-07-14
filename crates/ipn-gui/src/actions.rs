//! Per-device **action buttons**: a user-defined command attached to one member
//! (RDP into it, SSH to it, open its web UI), surfaced as a colored button on that
//! member's row and as an entry in the tray menu.
//!
//! Two things make this the one piece of member state the daemon knows nothing about:
//!
//! * It is **local to this machine**, not to the network. The command that reaches a
//!   device is a property of the machine you're sitting at (`mstsc` on Windows,
//!   `xfreerdp` on Linux), so sharing it through the signed roster would push a
//!   Windows command line onto a Mac. Notes and nicknames are local too — but they
//!   live in the daemon, and this deliberately does not, which is the second point.
//! * It is an **executable command line**. The daemon runs as SYSTEM/root and its IPC
//!   socket is reachable by every local user; storing exec strings there, to be
//!   spawned later by whichever user's GUI reads them back, would turn an inert local
//!   IPC surface into a cross-user code-execution path. A note is text; this is not.
//!   So it lives in the *user's own* config dir, writable only by the user whose GUI
//!   will run it, and never crosses the IPC boundary at all.
//!
//! Commands are spawned **directly** — no `cmd.exe`, no `sh -c`. The command line is
//! split on whitespace with double quotes grouping (and no backslash escapes, so
//! `C:\Windows\System32\mstsc.exe` survives), then each token is placeholder-expanded
//! individually. Expanding *after* the split is what keeps a device named `Media Box`
//! from silently becoming two arguments.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use ipn_ipc::MemberView;
use serde::{Deserialize, Serialize};

/// The 8 colors a user may pick from. One choice per action; the light/dark
/// variants are derived, never chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionColor {
    #[default]
    Blue,
    Teal,
    Green,
    Yellow,
    Orange,
    Red,
    Purple,
    Slate,
}

impl ActionColor {
    pub const ALL: [ActionColor; 8] = [
        Self::Blue,
        Self::Teal,
        Self::Green,
        Self::Yellow,
        Self::Orange,
        Self::Red,
        Self::Purple,
        Self::Slate,
    ];

    /// Stable id — the serialized form and the CSS class suffix.
    pub fn id(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Teal => "teal",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Orange => "orange",
            Self::Red => "red",
            Self::Purple => "purple",
            Self::Slate => "slate",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Self::Blue => "Blue",
            Self::Teal => "Teal",
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Orange => "Orange",
            Self::Red => "Red",
            Self::Purple => "Purple",
            Self::Slate => "Slate",
        }
    }

    /// The one colour the user actually picks: a **vivid** hue, theme-independent.
    /// It is the button's 1px border, and the swatch they choose it from. Everything
    /// else about the button is derived from it, so the palette is exactly these
    /// eight values.
    pub fn vivid(self) -> &'static str {
        match self {
            Self::Blue => "#3584e4",
            Self::Teal => "#21b5c4",
            Self::Green => "#33d17a",
            // A gold, not a lemon. A *pure* vivid yellow is only ~1.5:1 against a white
            // card — a border you cannot see. Yellow is simply too light to be both
            // maximally vivid and visible on white; this is the same corner it backed
            // us into over text colour.
            Self::Yellow => "#e5a50a",
            Self::Orange => "#ff7800",
            Self::Red => "#e01b24",
            Self::Purple => "#c061cb",
            Self::Slate => "#9a9996",
        }
    }

    /// The button's interior: the vivid colour tinted toward the surface it sits on —
    /// washed out toward white in light mode, sunk toward black in dark mode. The
    /// vivid border carries the identity; the fill only has to hint at it, which is
    /// what lets the *text* colour depend on the theme alone (see [`Self::text`]).
    ///
    /// The dark tint is deliberately *lighter* than the light one is pale (0.68 vs
    /// 0.82). Equal strengths are not equally visible: sinking a hue toward black
    /// kills its chroma much faster than washing it toward white does, so a symmetric
    /// pair leaves the dark fills reading as near-black.
    pub fn fill(self, dark: bool) -> String {
        self.tinted(dark, if dark { 0.68 } else { 0.82 })
    }

    /// Same, pulled a little closer to the vivid colour — the hover state.
    pub fn fill_hover(self, dark: bool) -> String {
        self.tinted(dark, if dark { 0.56 } else { 0.70 })
    }

    /// Same again, closer still — the pressed state.
    pub fn fill_active(self, dark: bool) -> String {
        self.tinted(dark, if dark { 0.46 } else { 0.60 })
    }

    /// Mix the vivid colour with black (dark mode) or white (light mode), `t` being
    /// how much of that neutral to take.
    fn tinted(self, dark: bool, t: f32) -> String {
        let hex = self.vivid();
        let ch = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f32;
        let neutral: f32 = if dark { 0.0 } else { 255.0 };
        let mix = |c: f32| (c * (1.0 - t) + neutral * t).round().clamp(0.0, 255.0) as u8;
        format!("#{:02x}{:02x}{:02x}", mix(ch(1)), mix(ch(3)), mix(ch(5)))
    }

    /// The text colour, which depends on the **theme and nothing else**.
    ///
    /// This is the rule the palette rests on. An earlier version chose the text colour
    /// per *hue* — white on everything, black on yellow — and yellow was visibly the
    /// one button that didn't belong. Tying it to the theme instead means every hue
    /// behaves identically, and the tint strengths above are chosen so that it always
    /// clears WCAG AA (enforced by a test).
    pub fn text(dark: bool) -> &'static str {
        if dark {
            "#ffffff"
        } else {
            "#000000"
        }
    }
}

/// One member's action button. `command` is stored verbatim (placeholders
/// unexpanded) so the editor round-trips exactly what the user typed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct DeviceAction {
    pub label: String,
    #[serde(default)]
    pub color: ActionColor,
    pub command: String,
    /// Run the command inside a terminal window rather than as a detached process.
    /// `default` (rather than a required field) so files written before this option
    /// existed still load.
    #[serde(default)]
    pub terminal: bool,
}

/// The whole file: NodeId hex → action. Versioned so a future format change can be
/// migrated rather than guessed at.
#[derive(Clone, Serialize, Deserialize)]
pub struct Actions {
    #[serde(default = "one")]
    version: u32,
    #[serde(default)]
    actions: HashMap<String, DeviceAction>,
}

fn one() -> u32 {
    1
}

/// Hand-written rather than derived: a derived `Default` would zero `version`, and
/// the *first* save on a machine starts from `Actions::default()` — which is exactly
/// how a fresh `actions.json` ended up stamped `"version": 0`, the one value the
/// format has never had.
impl Default for Actions {
    fn default() -> Self {
        Self {
            version: one(),
            actions: HashMap::new(),
        }
    }
}

/// `~/.config/Nullgate/actions.json` (and the platform equivalents) — the same
/// per-user config dir the window size already lives in.
pub fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("io.github", "steeb_k", "Nullgate")
        .map(|d| d.config_dir().join("actions.json"))
}

/// Modification time as a millisecond stamp, for cheap change detection. The tray
/// agent is a separate process from the GUI that edits this file, so it polls this
/// rather than holding a watcher: one `stat` a second beats a new dependency.
pub fn mtime_ms() -> u64 {
    let Some(p) = path() else { return 0 };
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Actions {
    /// Read the file. A missing file is simply "no actions"; a corrupt one is
    /// logged and treated the same, because losing a convenience button must never
    /// keep the app from starting.
    pub fn load() -> Self {
        let Some(p) = path() else { return Self::default() };
        let Ok(raw) = std::fs::read_to_string(&p) else {
            return Self::default();
        };
        match serde_json::from_str(&raw) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "actions file is unreadable; ignoring it");
                Self::default()
            }
        }
    }

    /// Write the file, pretty-printed: it is a plain, hand-editable JSON file in the
    /// user's own config dir, and someone will eventually edit it by hand.
    pub fn save(&self) -> Result<()> {
        let p = path().context("no config directory on this system")?;
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(&p, raw).with_context(|| format!("writing {}", p.display()))?;
        Ok(())
    }

    pub fn get(&self, node_id: &str) -> Option<&DeviceAction> {
        self.actions.get(node_id)
    }

    pub fn set(&mut self, node_id: &str, action: DeviceAction) {
        self.actions.insert(node_id.to_string(), action);
    }

    pub fn remove(&mut self, node_id: &str) {
        self.actions.remove(node_id);
    }
}

// ---------------------------------------------------------------------------
// Running one
// ---------------------------------------------------------------------------

/// Split a command line into argv. Double quotes group; **backslash is a literal**,
/// not an escape — otherwise every Windows path in the file would quietly lose its
/// separators. `""` yields an empty argument, which is how a caller writes one.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    // Distinguishes "no token here" from "a token that happens to be empty" (`""`).
    let mut started = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// The name we show for a member — the same fallback chain the member list uses.
fn display_name(m: &MemberView) -> String {
    m.label
        .clone()
        .or_else(|| m.hostname.clone())
        .unwrap_or_else(|| m.node_id.chars().take(8).collect())
}

/// Substitute the placeholders in a single argv token.
pub fn expand(token: &str, m: &MemberView) -> String {
    token
        .replace("{ip}", m.virtual_ip.as_deref().unwrap_or(""))
        .replace("{name}", &display_name(m))
        .replace("{hostname}", m.hostname.as_deref().unwrap_or(""))
        .replace("{node_id}", &m.node_id)
}

/// The placeholders, for the editor's help text.
pub const PLACEHOLDERS: &str = "{ip} · {name} · {hostname} · {node_id}";

/// Spawn the action's command as a new process.
///
/// With `terminal` unset the command is detached and its streams go nowhere — right
/// for a GUI program (`mstsc`, a browser), wrong for anything that wants a console.
/// With `terminal` set it is given a real terminal window instead; see
/// [`spawn_in_terminal`].
pub fn run(action: &DeviceAction, m: &MemberView) -> Result<()> {
    let tokens = tokenize(&action.command);
    let Some((program, args)) = tokens.split_first() else {
        bail!("This action has no command to run.");
    };
    // Catch the common case early with a sentence the user can act on, rather than
    // spawning `mstsc /v:` and letting it fail with its own dialog.
    if action.command.contains("{ip}") && m.virtual_ip.is_none() {
        bail!("{} has no Nullgate IP yet.", display_name(m));
    }

    let program = expand(program, m);
    let args: Vec<String> = args.iter().map(|a| expand(a, m)).collect();
    if action.terminal {
        spawn_in_terminal(&program, &args)
    } else {
        spawn_detached(&program, &args)
    }
}

/// Reap the child on a throwaway thread. Nothing waits on an action, but a GUI that
/// stays up for weeks and starts an RDP session a day would otherwise pile up
/// zombies on Unix.
fn reap(child: std::process::Child) {
    let mut child = child;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// A detached process with no streams — the default, and what a GUI program wants.
fn spawn_detached(program: &str, args: &[String]) -> Result<()> {
    let child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("couldn't start “{program}”"))?;
    reap(child);
    Ok(())
}

/// Windows: hand the child its own console with `CREATE_NEW_CONSOLE`.
///
/// Note what is *not* here: any redirection. Leaving stdio at `inherit` matters — a
/// GUI-subsystem process has no standard handles, so Rust omits
/// `STARTF_USESTDHANDLES` entirely and Windows wires the child's stdin/stdout/stderr
/// to the fresh console. Set them to `null()` (as [`spawn_detached`] does) and the
/// console window still appears but the program talks to `NUL` — a console you can
/// type into that shows nothing back.
#[cfg(windows)]
fn spawn_in_terminal(program: &str, args: &[String]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

    let child = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .with_context(|| format!("couldn't start “{program}”"))?;
    reap(child);
    Ok(())
}

/// The terminal emulators we know how to hand an argv to, in preference order, with
/// the flag each one uses to mean "everything after this is the command".
///
/// `-e` is not universal: `gnome-terminal -e` takes a single *string* and would drop
/// every argument after the program, so it needs `--` instead. The generic
/// `x-terminal-emulator` alias sits near the bottom for the same reason — on a Debian
/// box it may well point at a terminal whose `-e` behaves that way.
#[cfg(all(unix, not(target_os = "macos")))]
const TERMINALS: &[(&str, &[&str])] = &[
    ("ptyxis", &["--"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("alacritty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("kitty", &[]),
    ("foot", &[]),
    ("x-terminal-emulator", &["-e"]),
    ("xterm", &["-e"]),
];

/// Linux/BSD: find a terminal emulator and let *it* run the command.
#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_in_terminal(program: &str, args: &[String]) -> Result<()> {
    let on_path = |exe: &str| {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|d| d.join(exe).is_file()))
            .unwrap_or(false)
    };

    // $TERMINAL is the user's explicit answer to this exact question, so it wins.
    let explicit = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty());
    let (term, prefix): (&str, &[&str]) = match &explicit {
        Some(t) => (t.as_str(), &["-e"]),
        None => *TERMINALS
            .iter()
            .find(|(exe, _)| on_path(exe))
            .context(
                "couldn't find a terminal emulator. Install one (gnome-terminal, konsole, \
                 xfce4-terminal, alacritty, kitty, foot, xterm), or set $TERMINAL.",
            )?,
    };

    let child = std::process::Command::new(term)
        .args(prefix)
        .arg(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("couldn't start the terminal “{term}”"))?;
    reap(child);
    Ok(())
}

/// macOS: Terminal.app can only be handed a *file*, never an argv — so write a
/// throwaway script with every token single-quoted and open that.
///
/// This is the one place a shell is involved, and it is worth being precise about
/// why that is still not the shell we refused elsewhere: the quoting is *ours*, built
/// from the already-split argv, so a token containing `;` or `$(…)` becomes one
/// literal argument rather than a new command. What the config file cannot do is
/// reach through it.
#[cfg(target_os = "macos")]
fn spawn_in_terminal(program: &str, args: &[String]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// POSIX single-quoting: everything is literal inside `'…'`, and a `'` is closed,
    /// escaped, and reopened.
    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    let mut script = String::from("#!/bin/sh\n");
    script.push_str(&sh_quote(program));
    for a in args {
        script.push(' ');
        script.push_str(&sh_quote(a));
    }
    script.push('\n');

    // Unique per launch: two clicks in the same second must not race on one file.
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "nullgate-action-{}-{}.command",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o700) // Terminal.app will only run it if it's executable.
        .open(&path)
        .with_context(|| format!("couldn't write {}", path.display()))?;
    f.write_all(script.as_bytes())?;
    drop(f);

    let child = std::process::Command::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("couldn't open Terminal")?;
    reap(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(label: Option<&str>, ip: Option<&str>) -> MemberView {
        MemberView {
            node_id: "abcdef0123456789".into(),
            hostname: Some("workshop".into()),
            label: label.map(Into::into),
            note: None,
            virtual_ip: ip.map(Into::into),
            local_ip: None,
            public_ip: None,
            location: None,
            observed_addr: None,
            direct: None,
            online: true,
            last_seen: 0,
            is_self: false,
            is_originator_device: false,
            role: "peer".into(),
            access_disabled: false,
            hidden: false,
        }
    }

    #[test]
    fn tokenize_groups_quotes_and_keeps_backslashes() {
        assert_eq!(tokenize("mstsc /v:10.99.0.5"), ["mstsc", "/v:10.99.0.5"]);
        assert_eq!(
            tokenize(r#""C:\Program Files\app.exe" --host {ip}"#),
            [r"C:\Program Files\app.exe", "--host", "{ip}"]
        );
        assert_eq!(tokenize("  spaced   out  "), ["spaced", "out"]);
        assert_eq!(tokenize(""), Vec::<String>::new());
        // An explicitly empty argument is expressible.
        assert_eq!(tokenize(r#"prog "" tail"#), ["prog", "", "tail"]);
    }

    #[test]
    fn placeholders_expand_per_token_so_spaces_dont_split_args() {
        let m = member(Some("Media Box"), Some("10.99.0.7"));
        let toks: Vec<String> =
            tokenize("app --name={name} --to {ip}").iter().map(|t| expand(t, &m)).collect();
        // {name} contains a space and must still be ONE argument.
        assert_eq!(toks, ["app", "--name=Media Box", "--to", "10.99.0.7"]);
    }

    #[test]
    fn missing_ip_is_reported_not_spawned_blank() {
        let m = member(None, None);
        let a = DeviceAction {
            label: "RDP".into(),
            color: ActionColor::Blue,
            command: "mstsc /v:{ip}".into(),
            terminal: false,
        };
        let err = run(&a, &m).unwrap_err().to_string();
        assert!(err.contains("no Nullgate IP"), "{err}");
    }

    /// A fresh file must be stamped with the real format version, not a zero that no
    /// released format ever used (the derived `Default` did exactly that).
    #[test]
    fn a_fresh_file_carries_the_current_version() {
        let json = serde_json::to_string(&Actions::default()).unwrap();
        assert!(json.contains("\"version\":1"), "{json}");
    }

    /// An `actions.json` written before the terminal option existed must still load.
    #[test]
    fn terminal_defaults_to_false_for_files_without_it() {
        let json = r#"{"version":1,"actions":{"aaa":{
            "label":"RDP","color":"blue","command":"mstsc /v:{ip}"}}}"#;
        let cfg: Actions = serde_json::from_str(json).expect("old file should still parse");
        let a = cfg.get("aaa").expect("action present");
        assert!(!a.terminal);
        assert_eq!(a.color, ActionColor::Blue);
    }

    fn luminance(hex: &str) -> f64 {
        let c = |i: usize| {
            let v = u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * c(1) + 0.7152 * c(3) + 0.0722 * c(5)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Text must stay readable on every fill, in every state, in both themes — that is
    /// the whole rule the palette rests on. The fills are *derived* from the tint
    /// strengths, so a nudge to one of those constants (or a new hue that happens to be
    /// very light, like yellow) could silently push a state under the line.
    #[test]
    fn every_fill_carries_its_text_at_wcag_aa() {
        for color in ActionColor::ALL {
            for dark in [true, false] {
                let fg = ActionColor::text(dark);
                for (state, bg) in [
                    ("rest", color.fill(dark)),
                    ("hover", color.fill_hover(dark)),
                    ("active", color.fill_active(dark)),
                ] {
                    let ratio = contrast(&bg, fg);
                    assert!(
                        ratio >= 4.5,
                        "{} {state} ({}) is only {ratio:.2}:1 — {bg} vs {fg}",
                        color.display(),
                        if dark { "dark" } else { "light" },
                    );
                }
            }
        }
    }

    /// The border is the button's identity, so it has to stand off the surface it sits
    /// on — a vivid that vanishes into the dark card is a button with no edge.
    #[test]
    fn every_vivid_border_stands_off_both_surfaces() {
        // Roughly libadwaita's card colours in each theme.
        for (surface, name) in [("#ffffff", "light card"), ("#1e1e1e", "dark card")] {
            for color in ActionColor::ALL {
                let ratio = contrast(color.vivid(), surface);
                assert!(
                    ratio >= 1.6,
                    "{} border is only {ratio:.2}:1 against the {name}",
                    color.display(),
                );
            }
        }
    }

    /// The whole path — tokenize, expand, spawn a real process — against a program
    /// every host has. Proves an action actually *runs*, which the unit tests above
    /// only approach one step at a time.
    #[test]
    fn a_real_command_spawns() {
        let m = member(Some("box"), Some("10.99.0.5"));
        #[cfg(windows)]
        let command = "cmd /c exit 0";
        #[cfg(not(windows))]
        let command = "/bin/sh -c \"exit 0\"";
        let a = DeviceAction {
            label: "Ping".into(),
            color: ActionColor::Green,
            command: command.into(),
            terminal: false,
        };
        run(&a, &m).expect("the action should spawn");
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let m = member(None, Some("10.99.0.5"));
        let a = DeviceAction {
            label: "Nope".into(),
            color: ActionColor::Red,
            command: "definitely-not-a-real-program-xyz {ip}".into(),
            terminal: false,
        };
        assert!(run(&a, &m).is_err());
        // Deliberately not asserted in terminal mode: only Windows spawns the program
        // itself (so a bad name fails here). Linux and macOS spawn the *terminal*,
        // which starts fine and reports the missing program in its own window.
    }

    #[test]
    fn colors_round_trip_through_their_ids() {
        for c in ActionColor::ALL {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.id()));
            assert_eq!(serde_json::from_str::<ActionColor>(&json).unwrap(), c);
        }
    }
}
