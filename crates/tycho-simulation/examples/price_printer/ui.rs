use std::{str::FromStr, time::Instant};

use futures::StreamExt;
use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::{CheckedSub, One, Pow};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Flex, Layout, Margin, Rect},
    style::{palette::tailwind, Color, Modifier, Style, Stylize},
    text::Text,
    widgets::{
        Block, BorderType, Cell, Clear, HighlightSpacing, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
    },
    DefaultTerminal, Frame,
};
use tokio::{select, sync::mpsc::Receiver};
use tracing::{info, warn};
use tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes};
use tycho_simulation::protocol::models::{ProtocolComponent, Update};

const INFO_TEXT: [&str; 2] = [
    "(Esc) quit | (↑/↓) navigate | (↵) Toggle Quote | (d) Pool Details | (+/-) Adjust Quote | (z) Flip Direction | (=) Enter Amount",
    "(f) Filter by Protocol | (c) Clear Filter | Quote: Press ↵ to show/hide | Use +/- to adjust | = to enter custom amount",
];

const ITEM_HEIGHT: u16 = 3;

struct TableColors {
    buffer_bg: Color,
    header_bg: Color,
    header_fg: Color,
    row_fg: Color,
    selected_row_style_fg: Color,
    selected_column_style_fg: Color,
    selected_cell_style_fg: Color,
    normal_row_color: Color,
    alt_row_color: Color,
    footer_border_color: Color,
}

impl TableColors {
    const fn new(color: &tailwind::Palette) -> Self {
        Self {
            buffer_bg: tailwind::SLATE.c950,
            header_bg: color.c900,
            header_fg: tailwind::SLATE.c200,
            row_fg: tailwind::SLATE.c200,
            selected_row_style_fg: color.c400,
            selected_column_style_fg: color.c400,
            selected_cell_style_fg: color.c600,
            normal_row_color: tailwind::SLATE.c950,
            alt_row_color: tailwind::SLATE.c900,
            footer_border_color: color.c400,
        }
    }
}

#[derive(Clone)]
struct Data {
    component: ProtocolComponent,
    state: Box<dyn ProtocolSim>,
    name: String,
    tokens: String,
    price: String,
}

impl Data {
    const fn ref_array(&self) -> [&String; 4] {
        [&self.name, &self.component.protocol_system, &self.tokens, &self.price]
    }
}

pub struct App {
    state: TableState,
    show_popup: bool,
    show_details: bool,
    quote_amount: BigUint,
    zero2one: bool,
    items: Vec<Data>,
    filtered_items: Vec<Data>,
    rx: Receiver<Update>,
    scroll_state: ScrollbarState,
    colors: TableColors,
    input_mode: bool,
    input_buffer: String,
    filter_mode: bool,
    filter_protocol: String,
    available_protocols: Vec<String>,
}

impl App {
    pub fn new(rx: Receiver<Update>) -> Self {
        Self {
            state: TableState::default().with_selected(0),
            show_popup: false,
            show_details: false,
            quote_amount: BigUint::one(),
            zero2one: true,
            rx,
            scroll_state: ScrollbarState::new(0),
            colors: TableColors::new(&tailwind::BLUE),
            items: Vec::new(),
            filtered_items: Vec::new(),
            input_mode: false,
            input_buffer: String::new(),
            filter_mode: false,
            filter_protocol: String::new(),
            available_protocols: Vec::new(),
        }
    }

    fn current_items(&self) -> &Vec<Data> {
        if self.filter_protocol.is_empty() {
            &self.items
        } else {
            &self.filtered_items
        }
    }

    pub fn move_row(&mut self, direction: isize) {
        let current_items_len = self.current_items().len();
        if current_items_len == 0 {
            return;
        }

        // Get current decimals, if any
        let current_decimals = self.state.selected().and_then(|idx| {
            let current_items = self.current_items();
            if idx < current_items.len() {
                let comp = &current_items[idx].component;
                Some(if self.zero2one { comp.tokens[0].decimals } else { comp.tokens[1].decimals })
            } else {
                None
            }
        });

        // Calculate the new index based on direction
        let new_index = match self.state.selected() {
            Some(i) => {
                if direction.is_positive() {
                    (i + 1) % current_items_len
                } else {
                    (i + current_items_len - 1) % current_items_len
                }
            }
            None => 0,
        };

        // Update state and scroll position
        self.state.select(Some(new_index));
        self.scroll_state = self
            .scroll_state
            .position(new_index * usize::from(ITEM_HEIGHT));

        // Adjust quote amount if decimals have changed
        if let Some(prev_decimals) = current_decimals {
            let current_items = self.current_items();
            if new_index < current_items.len() {
                let comp = &current_items[new_index].component;
                let decimals = comp.tokens[if self.zero2one { 0 } else { 1 }].decimals;
                if decimals >= prev_decimals {
                    self.quote_amount *= BigUint::from(10u64).pow(decimals - prev_decimals);
                } else {
                    let new_amount = self.quote_amount.clone() /
                        BigUint::from(10u64).pow(prev_decimals - decimals);
                    self.quote_amount =
                        if new_amount > BigUint::ZERO { new_amount } else { BigUint::one() };
                }
            }
        }
    }

    pub fn update_data(&mut self, update: Update) {
        info!("Got {} new pairs", update.new_pairs.len());
        info!("Total pairs: {}", self.items.len());
        for (id, comp) in update.new_pairs.iter() {
            let name = format!("{comp_id:#042x}", comp_id = comp.id);
            let tokens = comp
                .tokens
                .iter()
                .map(|a| a.symbol.clone())
                .join("/");

            match update.states.get(id) {
                Some(state) => {
                    // Check if spot_price calculation is successful
                    match state.spot_price(&comp.tokens[0], &comp.tokens[1]) {
                        Ok(price) => {
                            self.items.push(Data {
                                component: comp.clone(),
                                state: state.clone(),
                                name,
                                tokens,
                                price: price.to_string(),
                            });
                        }
                        Err(e) => {
                            // Skip pools with spot_price errors
                            warn!(
                                "Skipping pool {comp_id} due to spot_price error: {e}",
                                comp_id = comp.id
                            );
                        }
                    }
                }
                None => {
                    warn!("Received update for unknown pool {comp_id}", comp_id = comp.id)
                }
            };
        }

        for (address, state) in update.states.iter() {
            let eth_address = Bytes::from_str(address).expect("Bad address");
            let entry = self
                .items
                .iter()
                .find_position(|e| e.component.id == eth_address);
            if let Some((index, _)) = entry {
                let row = &self.items[index];
                match state.spot_price(&row.component.tokens[0], &row.component.tokens[1]) {
                    Ok(price) => {
                        // Update the price and state if calculation is successful
                        let row = self.items.get_mut(index).unwrap();
                        row.price = price.to_string();
                        row.state = state.clone();
                    }
                    Err(_) => {
                        // Remove the pool if spot_price calculation fails
                        warn!("Removing pool {} due to spot_price error", address);
                        self.items.remove(index);
                    }
                }
            }
        }

        for comp in update.removed_pairs.values() {
            let entry = self
                .items
                .iter()
                .enumerate()
                .find(|(_, e)| e.component.id == comp.id);
            if let Some((idx, _)) = entry {
                self.items.remove(idx);
            }
        }

        // Update available protocols
        self.update_available_protocols();
        // Update filtered items if filter is active
        self.update_filtered_items();
    }

    fn update_available_protocols(&mut self) {
        let mut protocols: Vec<String> = self
            .items
            .iter()
            .map(|item| item.component.protocol_system.clone())
            .collect();
        protocols.sort_unstable();
        protocols.dedup();
        self.available_protocols = protocols;
    }

    fn update_filtered_items(&mut self) {
        if self.filter_protocol.is_empty() {
            self.filtered_items.clear();
        } else {
            self.filtered_items = self
                .items
                .iter()
                .filter(|item| item.component.protocol_system == self.filter_protocol)
                .cloned()
                .collect();
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> anyhow::Result<()> {
        let mut reader = event::EventStream::new();
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            select! {
                maybe_data = self.rx.recv() => {
                    if let Some(data) = maybe_data {
                        self.update_data(data);
                    }
                },
                maybe_event = reader.next() => {
                    if let Some(Ok(Event::Key(key))) = maybe_event {
                        if key.kind == KeyEventKind::Press {
                            if self.input_mode {
                                match key.code {
                                    KeyCode::Char(c) if c.is_ascii_digit() => {
                                        self.input_buffer.push(c);
                                    },
                                    KeyCode::Backspace => {
                                        self.input_buffer.pop();
                                    },
                                    KeyCode::Enter => {
                                        if let Ok(amount) = BigUint::from_str(&self.input_buffer) {
                                            self.quote_amount = amount;
                                        }
                                        self.input_mode = false;
                                        self.input_buffer.clear();
                                    },
                                    KeyCode::Esc => {
                                        self.input_mode = false;
                                        self.input_buffer.clear();
                                    },
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Char('q') | KeyCode::Esc => {
                                        if !self.show_popup && !self.show_details && !self.filter_mode {
                                            return Ok(())
                                        }
                                        self.show_popup = false;
                                        self.show_details = false;
                                        self.filter_mode = false;
                                    },
                                    KeyCode::Char('j') | KeyCode::Down => self.move_row(1),
                                    KeyCode::Char('+') => {
                                        self.modify_quote(true)
                                    },
                                    KeyCode::Char('-') => {
                                        self.modify_quote(false)
                                    },
                                    KeyCode::Char('z') => {
                                        self.zero2one = !self.zero2one;
                                        self.quote_amount = BigUint::one();
                                    }
                                    KeyCode::Char('k') | KeyCode::Up => self.move_row(-1),
                                    KeyCode::Char('=') => {
                                        self.input_mode = true;
                                        self.input_buffer.clear();
                                    },
                                    KeyCode::Char('d') => self.show_details = !self.show_details,
                                    KeyCode::Char('f') => self.filter_mode = !self.filter_mode,
                                    KeyCode::Char('c') => {
                                        self.filter_protocol.clear();
                                        self.update_filtered_items();
                                        self.state.select(Some(0));
                                    },
                                    KeyCode::Enter => {
                                        if self.filter_mode {
                                            self.select_filter();
                                        } else {
                                            self.show_popup = !self.show_popup;
                                        }
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            };
        }
    }

    fn select_filter(&mut self) {
        if !self.available_protocols.is_empty() {
            let selected_idx = self.state.selected().unwrap_or(0) % self.available_protocols.len();
            self.filter_protocol = self.available_protocols[selected_idx].clone();
            self.update_filtered_items();
            self.filter_mode = false;
            self.state.select(Some(0));
        }
    }

    fn modify_quote(&mut self, increase: bool) {
        if !self.show_popup {
            return;
        }

        if let Some(idx) = self.state.selected() {
            let current_items = self.current_items();
            if idx < current_items.len() {
                let comp = &current_items[idx].component;
                let decimals =
                    if self.zero2one { comp.tokens[0].decimals } else { comp.tokens[1].decimals };
                if increase {
                    self.quote_amount += BigUint::from(10u64).pow(decimals);
                } else {
                    self.quote_amount = self
                        .quote_amount
                        .checked_sub(&BigUint::from(10u64).pow(decimals))
                        .unwrap_or_else(BigUint::one);
                }
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let vertical = &Layout::vertical([Constraint::Min(5), Constraint::Length(4)]);
        let rects = vertical.split(frame.area());

        self.render_table(frame, rects[0]);
        self.render_scrollbar(frame, rects[0]);
        self.render_footer(frame, rects[1]);
        if self.items.is_empty() {
            self.render_loading(frame);
        }
        if self.show_popup {
            self.render_quote_popup(frame);
        }
        if self.show_details {
            self.render_details_popup(frame);
        }
        if self.filter_mode {
            self.render_filter_popup(frame);
        }
        if self.input_mode {
            self.render_input_popup(frame);
        }
    }

    fn render_table(&mut self, frame: &mut Frame, area: Rect) {
        let header_style = Style::default()
            .fg(self.colors.header_fg)
            .bg(self.colors.header_bg);
        let selected_row_style = Style::default()
            .add_modifier(Modifier::REVERSED)
            .fg(self.colors.selected_row_style_fg);
        let selected_col_style = Style::default().fg(self.colors.selected_column_style_fg);
        let selected_cell_style = Style::default()
            .add_modifier(Modifier::REVERSED)
            .fg(self.colors.selected_cell_style_fg);

        let header = ["Pool", "Protocol", "Tokens", "Price"]
            .into_iter()
            .map(Cell::from)
            .collect::<Row>()
            .style(header_style)
            .height(1);
        let current_items = self.current_items();
        let rows = current_items
            .iter()
            .enumerate()
            .map(|(i, data)| {
                let color = match i % 2 {
                    0 => self.colors.normal_row_color,
                    _ => self.colors.alt_row_color,
                };
                let item = data.ref_array();
                item.into_iter()
                    .map(|content| Cell::from(Text::from(format!("\n{content}\n"))))
                    .collect::<Row>()
                    .style(
                        Style::new()
                            .fg(self.colors.row_fg)
                            .bg(color),
                    )
                    .height(ITEM_HEIGHT)
            });
        let bar = " █ ";
        let t = Table::new(
            rows,
            [
                // + 1 is for padding.
                Constraint::Length(70),
                Constraint::Min(1),
                Constraint::Min(1),
                Constraint::Min(1),
            ],
        )
        .header(header)
        .row_highlight_style(selected_row_style)
        .column_highlight_style(selected_col_style)
        .cell_highlight_style(selected_cell_style)
        .highlight_symbol(Text::from(vec!["".into(), bar.into(), bar.into(), "".into()]))
        .bg(self.colors.buffer_bg)
        .highlight_spacing(HighlightSpacing::Always);
        frame.render_stateful_widget(t, area, &mut self.state);
    }

    fn render_scrollbar(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_stateful_widget(
            Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(Margin { vertical: 1, horizontal: 1 }),
            &mut self.scroll_state,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let info_footer = Paragraph::new(Text::from_iter(INFO_TEXT))
            .style(
                Style::new()
                    .fg(self.colors.row_fg)
                    .bg(self.colors.buffer_bg),
            )
            .centered()
            .block(
                Block::bordered()
                    .border_type(BorderType::Double)
                    .border_style(Style::new().fg(self.colors.footer_border_color)),
            );
        frame.render_widget(info_footer, area);

        // Render pool count on the bottom right
        let pool_count_text = format!(" Pools: {} ", self.items.len());
        let pool_count = Paragraph::new(pool_count_text)
            .style(
                Style::new()
                    .fg(self.colors.header_fg)
                    .bg(self.colors.buffer_bg),
            )
            .right_aligned();

        // Position in the bottom right corner, inside the border
        let count_area = Rect {
            x: area.x + area.width.saturating_sub(20),
            y: area.y + area.height.saturating_sub(1),
            width: 20.min(area.width),
            height: 1,
        };
        frame.render_widget(pool_count, count_area);
    }

    fn render_loading(&self, frame: &mut Frame) {
        let area = frame.area();

        let block = Block::bordered();
        let popup = Paragraph::new(Text::from("\nLOADING...\n"))
            .centered()
            .block(block);
        let area = popup_area(area, Constraint::Percentage(50), Constraint::Length(5));
        frame.render_widget(Clear, area);
        frame.render_widget(popup, area);
    }

    fn render_quote_popup(&self, frame: &mut Frame) {
        let area = frame.area();

        if let Some(idx) = self.state.selected() {
            if self.quote_amount > BigUint::ZERO {
                let current_items = self.current_items();
                if idx < current_items.len() {
                    let comp = &current_items[idx].component;
                    let state = &current_items[idx].state;
                    let (token_in, token_out) = if self.zero2one {
                        (&comp.tokens[0], &comp.tokens[1])
                    } else {
                        (&comp.tokens[1], &comp.tokens[0])
                    };

                    let start = Instant::now();
                    let res = state.get_amount_out(self.quote_amount.clone(), token_in, token_out);
                    let duration = start.elapsed();

                    let text = res
                    .map(|data| {
                        format!(
                            "Swap Direction: {token_in_symbol} → {token_out_symbol}\nQuote amount: {quote_amount}\nReceived amount: {amount}\nGas: {gas}\nDuration: {duration:?}",
                            token_in_symbol = token_in.symbol,
                            token_out_symbol = token_out.symbol,
                            quote_amount = self.quote_amount,
                            amount = data.amount,
                            gas = data.gas
                        )
                    })
                    .unwrap_or_else(|err| format!("{err:?}"));

                    let block = Block::bordered().title("Quote:");
                    let popup = Paragraph::new(Text::from(text))
                        .block(block)
                        .wrap(Wrap { trim: false });
                    let area =
                        popup_area(area, Constraint::Percentage(50), Constraint::Percentage(50));
                    frame.render_widget(Clear, area);
                    frame.render_widget(popup, area);
                }
            }
        }
    }

    fn render_input_popup(&self, frame: &mut Frame) {
        let area = frame.area();

        let text = format!(
            "Enter quote amount: {}\n\nPress Enter to confirm or Esc to cancel",
            self.input_buffer
        );
        let block = Block::bordered()
            .title("Quote Amount Input")
            .border_style(Style::new().fg(self.colors.footer_border_color));
        let input = Paragraph::new(Text::from(text))
            .block(block)
            .wrap(Wrap { trim: false })
            .style(
                Style::new()
                    .fg(self.colors.row_fg)
                    .bg(self.colors.buffer_bg),
            );
        let area = popup_area(area, Constraint::Percentage(50), Constraint::Length(8));
        frame.render_widget(Clear, area);
        frame.render_widget(input, area);
    }

    fn render_details_popup(&self, frame: &mut Frame) {
        let area = frame.area();

        if let Some(idx) = self.state.selected() {
            let current_items = self.current_items();
            if idx < current_items.len() {
                let comp = &current_items[idx].component;
                let mut text = format!(
                    "Pool ID: {:#042x}\nProtocol System: {}\nProtocol Type: {}\nChain: {:?}\nCreated At: {}\nCreation Tx: {:#042x}\n\nTokens:\n",
                    comp.id,
                    comp.protocol_system,
                    comp.protocol_type_name,
                    comp.chain,
                    comp.created_at.format("%Y-%m-%d %H:%M:%S"),
                    comp.creation_tx
                );

                for (i, token) in comp.tokens.iter().enumerate() {
                    text.push_str(&format!(
                        "  {}: {} ({}), decimals: {}\n",
                        i, token.symbol, token.address, token.decimals
                    ));
                }

                text.push_str("\nContract IDs:\n");
                for (i, contract_id) in comp.contract_ids.iter().enumerate() {
                    text.push_str(&format!("  {}: {:#042x}\n", i, contract_id));
                }

                if !comp.static_attributes.is_empty() {
                    text.push_str("\nStatic Attributes:\n");
                    for (key, value) in &comp.static_attributes {
                        text.push_str(&format!("  {}: {:#x}\n", key, value));
                    }
                }

                let block = Block::bordered()
                    .title("Pool Details")
                    .border_style(Style::new().fg(self.colors.footer_border_color));
                let popup = Paragraph::new(Text::from(text))
                    .block(block)
                    .wrap(Wrap { trim: false })
                    .style(
                        Style::new()
                            .fg(self.colors.row_fg)
                            .bg(self.colors.buffer_bg),
                    );
                let area = popup_area(area, Constraint::Percentage(80), Constraint::Percentage(80));
                frame.render_widget(Clear, area);
                frame.render_widget(popup, area);
            }
        }
    }

    fn render_filter_popup(&self, frame: &mut Frame) {
        let area = frame.area();

        let mut text = String::from("Select Protocol to Filter:\n\n");
        if self.available_protocols.is_empty() {
            text.push_str("No protocols available");
        } else {
            for (i, protocol) in self
                .available_protocols
                .iter()
                .enumerate()
            {
                let marker = if Some(i) == self.state.selected() { "► " } else { "  " };
                text.push_str(&format!("{}{}\n", marker, protocol));
            }
            text.push_str("\nPress Enter to select, Esc to cancel");
        }

        let block = Block::bordered()
            .title("Protocol Filter")
            .border_style(Style::new().fg(self.colors.footer_border_color));
        let popup = Paragraph::new(Text::from(text))
            .block(block)
            .wrap(Wrap { trim: false })
            .style(
                Style::new()
                    .fg(self.colors.row_fg)
                    .bg(self.colors.buffer_bg),
            );
        let area = popup_area(area, Constraint::Percentage(60), Constraint::Percentage(60));
        frame.render_widget(Clear, area);
        frame.render_widget(popup, area);
    }
}

/// helper function to create a centered rect using up certain percentage of the available rect `r`
fn popup_area(area: Rect, x: Constraint, y: Constraint) -> Rect {
    let vertical = Layout::vertical([y]).flex(Flex::Center);
    let horizontal = Layout::horizontal([x]).flex(Flex::Center);
    let [area] = vertical.areas(area);
    let [area] = horizontal.areas(area);
    area
}
