//! autochord — a keyboard-driven chord synth controller in the terminal.
//!
//! Play chords from the piano keys, shape them with quality/addition buttons
//! and the two voicing dials, and see the sounding notes on a piano with the
//! chord name above it. Key handling and the latch model live in `app::on_key`.

mod app;
mod audio;
mod control;
mod notes;
mod synth;
mod transport;

use std::io::{self, Stdout};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;
use audio::SynthEvent;
use synth::VoiceMonitor;
use transport::Transport;

/// UI refresh / input-poll interval (~60 Hz).
const TICK: Duration = Duration::from_millis(16);

/// The agent skill, installed into `.claude/skills/autochord/` by the
/// `install-skill` subcommand.
const SKILL: &str = include_str!("../skill/SKILL.md");

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Non-TUI subcommands: the text control interface for agents/scripts.
    match args.first().map(String::as_str) {
        Some("state") => {
            let pid = args.get(1).and_then(|s| s.parse().ok());
            print!("{}", control::cli_state(pid));
            return Ok(());
        }
        Some("send") => {
            let Some(pid) = args.get(1).and_then(|s| s.parse::<u32>().ok()) else {
                eprintln!("usage: autochord send <pid> <key> <value>");
                std::process::exit(2);
            };
            let command = args[2..].join(" ");
            control::cli_send(pid, &command)?;
            return Ok(());
        }
        Some("ls") => {
            for pid in control::live_instances() {
                println!("{pid}");
            }
            return Ok(());
        }
        Some("patches") => {
            for (i, (name, _)) in synth::presets().iter().enumerate() {
                println!("{i}\t{name}");
            }
            return Ok(());
        }
        Some("install-skill") => {
            install_skill()?;
            return Ok(());
        }
        Some("help" | "--help" | "-h") => {
            print_help();
            return Ok(());
        }
        _ => {}
    }

    // Just intonation is on by default; disable it for plain 12-TET.
    let just = !args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--et" | "--equal" | "--equal-temperament" | "--no-just"
        )
    });

    // Shared, lock-free view of the synth's live voices — the UI reads this to
    // light up keys, so it always mirrors what the synth is actually playing.
    let monitor = Arc::new(VoiceMonitor::new());

    // Start audio first so the UI can show the real device details.
    let (tx, rx) = mpsc::channel::<SynthEvent>();
    let (_stream, audio_info) = audio::start(rx, monitor.clone())?;

    // Does this terminal report key-release events?
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));

    install_panic_hook(enhanced);
    let mut terminal = setup_terminal(enhanced)?;

    let mut app = App::new(tx, audio_info, enhanced, just, monitor, Transport::new());
    let result = run(&mut terminal, &mut app);

    restore_terminal(enhanced)?;
    app.shutdown(); // remove this instance's control files
    // `_stream` drops here, stopping audio.
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| app::render(app, frame))?;

        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                app.on_key(key);
            }
        }
        app.tick(); // advance the arpeggiator clock
        app.serve(); // publish state / apply queued agent commands
    }
    Ok(())
}

/// Write the agent skill into `./.claude/skills/autochord/SKILL.md`.
fn install_skill() -> Result<()> {
    let dir = std::path::Path::new(".claude/skills/autochord");
    std::fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL)?;
    println!("installed skill -> {}", path.display());
    Ok(())
}

fn print_help() {
    println!(
        "autochord — terminal chord synth\n\
         \n\
         autochord                 run the TUI\n\
         autochord --et            run in 12-TET (no just intonation)\n\
         autochord state [pid]     print live state (all instances, or one)\n\
         autochord send <pid> ...  send a `key value` command to an instance\n\
         autochord ls              list running instance PIDs\n\
         autochord patches         list the built-in synth presets\n\
         autochord install-skill   write the agent skill into ./.claude/skills\n\
         \n\
         State/commands live as text files in {}",
        control::dir().display()
    );
}

fn setup_terminal(enhanced: bool) -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    if enhanced {
        // REPORT_EVENT_TYPES gives us Press/Repeat/Release; the escape-code
        // flag makes even plain keys report releases.
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )?;
    }
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(enhanced: bool) -> Result<()> {
    let mut stdout = io::stdout();
    if enhanced {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    execute!(stdout, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

/// Restore the terminal on panic so a crash doesn't leave a wrecked shell.
fn install_panic_hook(enhanced: bool) {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(enhanced);
        original(info);
    }));
}
