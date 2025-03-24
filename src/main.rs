use std::{io, sync::mpsc::{self, Sender}, thread, time};

use clap::Parser as ClapParser;
use event::{KeyEventKind, MouseEventKind};
use ferrisetw::{parser::Parser, provider::Provider, EventRecord, SchemaLocator};
use log::LevelFilter;
use ratatui::{prelude::*, widgets::*};
use sha1::{Digest, Sha1};
use tui_logger::*;
use windows_core::{GUID, HSTRING};

use self::crossterm_backend::*;

#[derive(ClapParser, Debug)]
struct Args {
    #[arg(short, long)]
    provider: Vec<String>,
}

struct App {
    mode: AppMode,
    states: Vec<TuiWidgetState>,
    tab_names: Vec<&'static str>,
    selected_tab: usize,
    progress_counter: Option<u16>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum AppMode {
    #[default]
    Run,
    Quit,
}

#[derive(Debug)]
enum AppEvent {
    UiEvent(Event),
    CounterChanged(Option<u16>),
    EtwEvent,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    init_logger(LevelFilter::Trace)?;
    set_default_level(LevelFilter::Trace);

    let mut terminal = init_terminal()?;
    terminal.clear()?;
    terminal.hide_cursor()?;

    App::new().start(&mut terminal, &args.provider)?;

    restore_terminal()?;
    terminal.clear()?;

    Ok(())
}

impl App {
    pub fn new() -> App {
        let states = vec![
            TuiWidgetState::new().set_default_display_level(LevelFilter::Info),
            TuiWidgetState::new().set_default_display_level(LevelFilter::Info),
            TuiWidgetState::new().set_default_display_level(LevelFilter::Info),
            TuiWidgetState::new().set_default_display_level(LevelFilter::Info),
        ];

        // Adding this line had provoked the bug as described in issue #69
        // let states = states.into_iter().map(|s| s.set_level_for_target("some::logger", LevelFilter::Off)).collect();
        let tab_names = vec!["State 1", "State 2", "State 3", "State 4"];
        App {
            mode: AppMode::Run,
            states,
            tab_names,
            selected_tab: 0,
            progress_counter: None,
        }
    }

    fn etw_callback(event: &EventRecord, schema_locator: &SchemaLocator, event_sender: &Sender<AppEvent>) {
        let Ok(schema) = schema_locator.event_schema(event) else {
            return;
        };

        let parser = Parser::create(event, &schema);

        let event_name = event.event_name(); //schema.task_name();
        let initial_props = if event_name.is_empty() {
            vec![]
        } else {
            vec![event_name]
        };

        let props: Vec<String> = initial_props
            .into_iter()
            .chain(schema.properties().iter().map(|prop| {
                match parser
                .try_parse::<String>(&prop.name) {
                    Ok(value) => format!(
                        "{}: {}",
                        prop.name,
                        value
                    ),
                    Err(_) => format!(
                        "{}: {}",
                        prop.name,
                        "Error parsing value"
                    ),
                }
                
            }))
            .collect();

        let message = props.join(", ");
        let provider = schema.provider_name();

        match event.level() {
            0 => (),
            1 => log::error!(target: &provider, "{}", message),
            2 => log::error!(target: &provider, "{}", message),
            3 => log::warn!(target: &provider, "{}", message),
            4 => log::info!(target: &provider, "{}", message),
            5 => log::debug!(target: &provider, "{}", message),
            _ => (),
        }

        //let _ = event_sender.send(AppEvent::EtwEvent);
    }

    pub fn start(
        mut self,
        terminal: &mut Terminal<impl Backend>,
        provider_names: &Vec<String>,
    ) -> anyhow::Result<()> {
        // Use an mpsc::channel to combine stdin events with app events
        let (tx, rx) = mpsc::channel();
        let event_tx = tx.clone();
        let progress_tx = tx.clone();

        let providers = provider_names
            .iter()
            .map(|name| {
                let etw_tx = tx.clone();
                let guid = if name.contains(".") {
                    tracelogging_to_guid(name).to_u128()
                } else {
                    GUID::try_from(name.as_str()).unwrap().to_u128()
                };
                Provider::by_guid(guid)
                    .add_callback(move |event, schema_locator| Self::etw_callback(event, schema_locator, &etw_tx))
                    .build()
            })
            .collect::<Vec<_>>();

        let mut builder = ferrisetw::UserTrace::new().named("TraceLogging-Viewer".to_string());

        for provider in providers {
            builder = builder.enable(provider);
        }

        let _trace = builder.start_and_process().unwrap();

        thread::spawn(move || input_thread(event_tx));
        //thread::spawn(move || progress_task(progress_tx).unwrap());
        //thread::spawn(move || background_task());

        self.run(terminal, rx)
    }

    /// Main application loop
    fn run(
        &mut self,
        terminal: &mut Terminal<impl Backend>,
        rx: mpsc::Receiver<AppEvent>,
    ) -> anyhow::Result<()> {
        for event in rx {
            match event {
                AppEvent::UiEvent(event) => self.handle_ui_event(event),
                AppEvent::CounterChanged(value) => self.update_progress_bar(event, value),
                AppEvent::EtwEvent => ()
            }
            if self.mode == AppMode::Quit {
                break;
            }
            self.draw(terminal)?;
        }
        Ok(())
    }

    fn update_progress_bar(&mut self, event: AppEvent, value: Option<u16>) {
        self.progress_counter = value;
        if value.is_none() {}
    }

    fn handle_ui_event(&mut self, event: Event) {
        let state = self.selected_state();

        // TODO: Could be nice to scroll in a shorter-than-page increments. Same is true for keyboard.
        //       Also, the scroll is currently in 10-events increments, which could differ greatly between events.
        if let Event::Mouse(mouse) = event {
            match mouse.kind {
                MouseEventKind::ScrollUp => state.transition(TuiWidgetEvent::PrevPageKey),
                MouseEventKind::ScrollDown => state.transition(TuiWidgetEvent::NextPageKey),
                _ => (),
            }
        }

        if let Event::Key(key) = event {
            // Ignore key release event
            if matches!(key.kind, KeyEventKind::Release) {
                return;
            }

            let code = key.code;

            match code.into() {
                Key::Char('q') => self.mode = AppMode::Quit,
                Key::Char('\t') => self.next_tab(),
                Key::BackTab => self.prev_tab(),
                Key::Tab => self.next_tab(),
                Key::Char(' ') => state.transition(TuiWidgetEvent::SpaceKey),
                Key::Esc => state.transition(TuiWidgetEvent::EscapeKey),
                Key::PageUp => state.transition(TuiWidgetEvent::PrevPageKey),
                Key::PageDown => state.transition(TuiWidgetEvent::NextPageKey),
                Key::Up => state.transition(TuiWidgetEvent::UpKey),
                Key::Down => state.transition(TuiWidgetEvent::DownKey),
                Key::Left => state.transition(TuiWidgetEvent::LeftKey),
                Key::Right => state.transition(TuiWidgetEvent::RightKey),
                Key::Char('+') => state.transition(TuiWidgetEvent::PlusKey),
                Key::Char('-') => state.transition(TuiWidgetEvent::MinusKey),
                Key::Char('h') => state.transition(TuiWidgetEvent::HideKey),
                Key::Char('f') => state.transition(TuiWidgetEvent::FocusKey),
                _ => (),
            }
        }
    }

    fn selected_state(&mut self) -> &mut TuiWidgetState {
        &mut self.states[self.selected_tab]
    }

    fn next_tab(&mut self) {
        self.selected_tab = (self.selected_tab + 1) % self.tab_names.len();
    }

    fn prev_tab(&mut self) {
        self.selected_tab = (self.selected_tab - 1) % self.tab_names.len();
    }

    fn draw(&mut self, terminal: &mut Terminal<impl Backend>) -> anyhow::Result<()> {
        terminal.draw(|frame| {
            frame.render_widget(self, frame.area());
        })?;
        Ok(())
    }
}

/// A simulated task that sends a counter value to the UI ranging from 0 to 100 every second.
fn progress_task(tx: mpsc::Sender<AppEvent>) -> anyhow::Result<()> {
    for progress in 0..100 {
        tx.send(AppEvent::CounterChanged(Some(progress)))?;

        thread::sleep(time::Duration::from_millis(1000));
    }
    tx.send(AppEvent::CounterChanged(None))?;
    Ok(())
}

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [tabs_area, smart_area, help_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Fill(50),
            Constraint::Length(3),
        ])
        .areas(area);

        Tabs::new(self.tab_names.iter().cloned())
            .block(Block::default().title("States").borders(Borders::ALL))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .select(self.selected_tab)
            .render(tabs_area, buf);

        TuiLoggerSmartWidget::default()
            .style_error(Style::default().fg(Color::Red))
            .style_debug(Style::default().fg(Color::Green))
            .style_warn(Style::default().fg(Color::Yellow))
            .style_trace(Style::default().fg(Color::Magenta))
            .style_info(Style::default().fg(Color::Cyan))
            .output_separator('|')
            .output_timestamp(Some("%H:%M:%S%.6f".to_string()))
            .output_level(Some(TuiLoggerLevelOutput::Long))
            .output_target(true)
            .output_file(false)
            .output_line(false)
            .state(self.selected_state())
            .render(smart_area, buf);

        if area.width > 40 {
            Text::from(vec![
                "Q: Quit | Tab: Switch state | ↑/↓: Select target | f: Focus target".into(),
                "←/→: Display level | +/-: Filter level | Space: Toggle hidden targets".into(),
                "h: Hide target selector | PageUp/Down: Scroll | Esc: Cancel scroll".into(),
            ])
            .style(Color::Gray)
            .centered()
            .render(help_area, buf);
        }
    }
}

/// A module for crossterm specific code
mod crossterm_backend {
    use super::*;

    pub use crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode as Key},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };

    pub fn init_terminal() -> io::Result<Terminal<impl Backend>> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(io::stdout());
        Terminal::new(backend)
    }

    pub fn restore_terminal() -> io::Result<()> {
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)
    }

    pub fn input_thread(tx_event: mpsc::Sender<AppEvent>) -> anyhow::Result<()> {
        while let Ok(event) = event::read() {
            tx_event.send(AppEvent::UiEvent(event))?;
        }
        Ok(())
    }
}

fn tracelogging_to_guid(provider_name: &str) -> GUID {
    const SIGNATURE: &[u8] = &[
        0x48, 0x2C, 0x2D, 0xB2, 0xC3, 0x90, 0x47, 0xC8, 0x87, 0xF8, 0x1A, 0x15, 0xBF, 0xC1, 0x30,
        0xFB,
    ];

    let unicode_provider_name_upper = HSTRING::from(provider_name.to_uppercase());
    let unicode_provider_name_upper_be = unicode_provider_name_upper
        .as_wide()
        .iter()
        .map(|c| c.swap_bytes())
        .collect::<Vec<u16>>();

    let mut hasher = Sha1::new();
    hasher.update(SIGNATURE);
    hasher.update(unsafe {
        std::slice::from_raw_parts(
            unicode_provider_name_upper_be.as_ptr() as *const u8,
            unicode_provider_name_upper_be.len() * 2,
        )
    });

    let hash = hasher.finalize();

    let first = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]);

    let second = u16::from_le_bytes([hash[4], hash[5]]);
    let hash_7 = (hash[7] & 0x0F) | 0x50;
    let third = u16::from_le_bytes([hash[6], hash_7]);

    GUID::from_values(
        first,
        second,
        third,
        [
            hash[8], hash[9], hash[10], hash[11], hash[12], hash[13], hash[14], hash[15],
        ],
    )
}

#[cfg(test)]
mod test {
    use windows_core::GUID;

    use crate::tracelogging_to_guid;

    #[test]
    fn test_tracelogging_to_guid() {
        assert_eq!(
            tracelogging_to_guid("MyCompany.MyComponent"),
            GUID::from_u128(0xce5fa4ea_ab00_5402_8b76_9f76ac858fb5)
        );
        assert_eq!(
            tracelogging_to_guid("Microsoft.RedSea"),
            GUID::from_u128(0xfcaa0b55_b416_5cc7_37a3_0e02bd9b42aa)
        );
    }
}
