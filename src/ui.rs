use crate::app::{App, HistBucket, Level, LinkState, EVENTS_CAP};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, Gauge, GraphType, List, ListItem, Paragraph, Row,
    Sparkline, Table,
};
use ratatui::Frame;

const ACCENT: Color = Color::Indexed(39);
const MUTED: Color = Color::Indexed(240);
const OK: Color = Color::Indexed(114);
const WARN: Color = Color::Indexed(179);
const BAD: Color = Color::Indexed(174);
const DIM: Color = Color::Indexed(236);

fn header_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            format!(" {} ", title),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn fit_spark(data: &[u64], width: u16) -> Vec<u64> {
    let w = width as usize;
    if w == 0 {
        return Vec::new();
    }
    if data.is_empty() {
        return vec![0; w];
    }
    let n = data.len();
    if n == 1 {
        return vec![data[0]; w];
    }
    (0..w)
        .map(|i| {
            let idx = (i * n) / w;
            data[idx.min(n - 1)]
        })
        .collect()
}

fn inner_w(area: Rect) -> u16 {
    area.width.saturating_sub(2).max(1)
}

fn state_color(s: LinkState) -> Color {
    match s {
        LinkState::Up => OK,
        LinkState::Degraded => WARN,
        LinkState::Down => BAD,
    }
}

fn state_label(s: LinkState) -> &'static str {
    match s {
        LinkState::Up => "● Online",
        LinkState::Degraded => "◐ Degraded",
        LinkState::Down => "○ Down",
    }
}

fn score_color(s: f32) -> Color {
    if s >= 75.0 {
        OK
    } else if s >= 40.0 {
        WARN
    } else {
        BAD
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let size = f.area();
    let show_targets = !app.primaries.is_empty();
    let show_heatmap = size.height >= 36;
    let show_extras = !app.extras.is_empty() && size.height >= 30;
    let show_stats = size.height >= 26;

    let mut constraints = vec![Constraint::Length(3), Constraint::Length(9)];
    if show_targets {
        let rows = app.primaries.len() as u16 + 4;
        constraints.push(Constraint::Length(rows));
    }
    if show_heatmap {
        constraints.push(Constraint::Length(5));
    }
    constraints.extend(vec![
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length((app.dns.resolvers.len() as u16) + 3),
    ]);
    if show_extras {
        constraints.push(Constraint::Length((app.extras.len() as u16) + 2));
    }
    if show_stats {
        constraints.push(Constraint::Length(5));
    }
    constraints.extend(vec![Constraint::Min(4), Constraint::Length(2)]);

    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(size);

    let mut idx = 0;
    draw_header(f, app, main[idx]);
    idx += 1;
    draw_latency(f, app, main[idx]);
    idx += 1;
    if show_targets {
        draw_targets(f, app, main[idx]);
        idx += 1;
    }
    if show_heatmap {
        draw_heatmap(f, app, main[idx]);
        idx += 1;
    }
    draw_loss(f, app, main[idx]);
    idx += 1;
    draw_jitter(f, app, main[idx]);
    idx += 1;
    draw_dns(f, app, main[idx]);
    idx += 1;
    if show_extras {
        draw_extras(f, app, main[idx]);
        idx += 1;
    }
    if show_stats {
        draw_stats(f, app, main[idx]);
        idx += 1;
    }
    draw_events(f, app, main[idx]);
    idx += 1;
    draw_footer(f, app, main[idx]);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);

    let left = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED));
    let primary_list: Vec<String> = app
        .primaries
        .iter()
        .map(|p| format!("{}:{}:{}", p.label, p.host, p.port))
        .collect();
    let state_line = Line::from(vec![
        Span::styled(
            " ping_monitor ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " [{}] dns {}×{} remind {}s ",
            primary_list.join(", "),
            app.dns.resolvers.len(),
            app.dns.names.len(),
            app.cfg.reminder_interval.as_secs()
        )),
        Span::raw(" "),
        Span::styled(
            state_label(app.state),
            Style::default()
                .fg(state_color(app.state))
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(left.title(state_line), h[0]);

    let score = app.score();
    let sc = score_color(score);
    let gauge = Gauge::default()
        .block(header_block("Quality Score"))
        .gauge_style(Style::default().fg(sc).bg(Color::Black))
        .percent((score as u16).min(100))
        .label(Span::styled(
            format!("{:>3.0}/100", score),
            Style::default()
                .fg(Color::Black)
                .bg(sc)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(gauge, h[1]);
}

fn draw_latency(f: &mut Frame, app: &App, area: Rect) {
    let samples: Vec<Option<f64>> = app.lat_hist.as_vec();
    let points: Vec<(f64, f64)> = samples
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.map(|x| (i as f64, x)))
        .collect();
    let ring_len = samples.len();
    let data_max = points.iter().map(|(_, y)| *y).fold(0.0_f64, f64::max);
    let mut y_max = data_max * 1.2;
    let warn_floor = app.cfg.latency_warn_ms * 0.5;
    if y_max < warn_floor {
        y_max = warn_floor;
    }
    let y_max = (y_max / 10.0).ceil() * 10.0;

    let stat = app.pooled_ping_stat();
    let cur = app.last_value_view().unwrap_or(0.0);
    let lwarn = app.cfg.latency_warn_ms;
    let lbad = app.cfg.latency_bad_ms;
    let lc = if cur > lbad {
        BAD
    } else if cur > lwarn {
        WARN
    } else {
        OK
    };
    let title = format!(
        "Latency  pooled  cur {:.1} ms   avg {:.1}   min {:.1}   max {:.1}   (median of {} targets)",
        cur, stat.avg().unwrap_or(0.0), stat.min, stat.max, app.primaries.len(),
    );

    let show_warn = app.cfg.latency_warn_ms <= y_max;
    let warn_pts: Vec<(f64, f64)> = if show_warn {
        (0..ring_len.max(1))
            .map(|i| (i as f64, app.cfg.latency_warn_ms))
            .collect()
    } else {
        Vec::new()
    };
    let mut datasets: Vec<Dataset> = Vec::new();
    if show_warn {
        datasets.push(
            Dataset::default()
                .name("warn")
                .marker(Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(DIM))
                .data(&warn_pts),
        );
    }
    datasets.push(
        Dataset::default()
            .name("pooled RTT")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(lc))
            .data(&points),
    );
    let chart = Chart::new(datasets)
        .block(header_block(&title))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(MUTED))
                .bounds([0.0, ring_len.max(1) as f64])
                .labels(vec![
                    Span::styled("older", Style::default().fg(MUTED)),
                    Span::styled("now", Style::default().fg(MUTED)),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(MUTED))
                .bounds([0.0, y_max.max(1.0)])
                .labels(vec![
                    Span::styled("0", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{}", (y_max / 2.0) as u64),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(format!("{}", y_max as u64), Style::default().fg(MUTED)),
                ]),
        );
    f.render_widget(chart, area);
}

fn draw_loss(f: &mut Frame, app: &App, area: Rect) {
    let max_len = app
        .primaries
        .iter()
        .map(|p| p.loss_ring.buf.len())
        .max()
        .unwrap_or(0);
    let mut agg = vec![0u64; max_len];
    for p in &app.primaries {
        let v = p.loss_ring.as_vec();
        for (i, x) in v.iter().enumerate() {
            if *x > 0.0 {
                agg[i] += 1;
            }
        }
    }
    let lpct = app.loss_pct();
    let window_losses: u64 = agg.iter().copied().filter(|v| *v > 0).sum();
    let total_losses_window = agg.iter().map(|v| *v as f64).sum::<f64>() as u64;
    let verdict = if lpct >= app.cfg.down_loss_pct {
        "  ⚠ HIGH"
    } else if lpct > 0.0 {
        "  ◐ loss"
    } else {
        ""
    };
    let title =
        format!(
        "Packet Loss  pooled  window {} loss-events   {} targets/sample peak   total {} ({:.2}%){}",
        window_losses, total_losses_window, app.pooled_lost(), lpct, verdict,
    );
    f.render_widget(
        Sparkline::default()
            .block(header_block(&title))
            .data(fit_spark(&agg, inner_w(area)))
            .max(app.primaries.len().max(1) as u64)
            .style(Style::default().fg(ACCENT)),
        area,
    );
}

fn draw_jitter(f: &mut Frame, app: &App, area: Rect) {
    let samples: Vec<f64> = app.jit_hist.as_vec();
    let ring_len = samples.len();
    let points: Vec<(f64, f64)> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v))
        .collect();
    let data_max = samples.iter().fold(0.0_f64, |a, &b| a.max(b));
    let mut y_max = data_max * 1.3;
    let warn_floor = app.cfg.jitter_warn_ms * 0.5;
    if y_max < warn_floor {
        y_max = warn_floor;
    }
    let y_max = (y_max / 10.0).ceil() * 10.0;

    let jwarn = app.cfg.jitter_warn_ms;
    let jcur = app.jitter_view();
    let lc = if jcur > jwarn * 2.0 {
        BAD
    } else if jcur > jwarn {
        WARN
    } else {
        OK
    };
    let verdict = if jcur > jwarn * 2.0 {
        "  ⚠ HIGH"
    } else if jcur > jwarn {
        "  ◐ elevated"
    } else {
        ""
    };
    let title = format!(
        "Jitter  pooled  cur {:.1} ms  (median of {} targets){}",
        jcur,
        app.primaries.len(),
        verdict,
    );

    let show_warn = app.cfg.jitter_warn_ms <= y_max;
    let warn_pts: Vec<(f64, f64)> = if show_warn {
        (0..ring_len.max(1))
            .map(|i| (i as f64, app.cfg.jitter_warn_ms))
            .collect()
    } else {
        Vec::new()
    };
    let mut datasets: Vec<Dataset> = Vec::new();
    if show_warn {
        datasets.push(
            Dataset::default()
                .name("warn")
                .marker(Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(DIM))
                .data(&warn_pts),
        );
    }
    datasets.push(
        Dataset::default()
            .name("pooled jitter")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(lc))
            .data(&points),
    );
    let chart = Chart::new(datasets)
        .block(header_block(&title))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(MUTED))
                .bounds([0.0, ring_len.max(1) as f64])
                .labels(vec![
                    Span::styled("older", Style::default().fg(MUTED)),
                    Span::styled("now", Style::default().fg(MUTED)),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(MUTED))
                .bounds([0.0, y_max.max(1.0)])
                .labels(vec![
                    Span::styled("0", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{}", (y_max / 2.0) as u64),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(format!("{}", y_max as u64), Style::default().fg(MUTED)),
                ]),
        );
    f.render_widget(chart, area);
}

fn draw_dns(f: &mut Frame, app: &App, area: Rect) {
    let warn = app.cfg.dns_warn_ms;
    let bad = app.cfg.dns_bad_ms;

    let header_cells: Vec<Cell> = std::iter::once(Cell::from(Span::styled(
        " resolver",
        Style::default().fg(MUTED),
    )))
    .chain(app.dns.names.iter().map(|n| {
        Cell::from(Span::styled(
            format!(" {}", short_name(n)),
            Style::default().fg(MUTED),
        ))
    }))
    .chain(std::iter::once(Cell::from(Span::styled(
        " row-avg",
        Style::default().fg(MUTED),
    ))))
    .collect();
    let header = Row::new(header_cells).height(1).bottom_margin(0);

    let mut rows: Vec<Row> = Vec::new();
    for (r_idx, (r_label, _)) in app.dns.resolvers.iter().enumerate() {
        let mut cells: Vec<Cell> = Vec::new();
        cells.push(Cell::from(Span::styled(
            format!(" {}", r_label),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        let mut sum = 0.0;
        let mut n = 0;
        for d_idx in 0..app.dns.names.len() {
            let c = &app.dns.cells[r_idx][d_idx];
            let (dot, sc, txt) = match c.last {
                Some(v) => {
                    sum += v;
                    n += 1;
                    let col = if v >= bad {
                        BAD
                    } else if v >= warn {
                        WARN
                    } else {
                        OK
                    };
                    let dot = if v >= bad {
                        "○"
                    } else if v >= warn {
                        "◐"
                    } else {
                        "●"
                    };
                    (dot, col, format!(" {:.0}ms", v))
                }
                None => ("✗", BAD, "  fail".to_string()),
            };
            cells.push(Cell::from(Line::from(vec![
                Span::styled(dot, Style::default().fg(sc)),
                Span::styled(txt, Style::default().fg(sc)),
            ])));
        }
        let avg_str = if n > 0 {
            format!(" {:.0}ms", sum / n as f64)
        } else {
            "  —  ".into()
        };
        let avg = if n > 0 { sum / n as f64 } else { 0.0 };
        let avg_col = if n == 0 || avg >= bad {
            BAD
        } else if avg >= warn {
            WARN
        } else {
            OK
        };
        cells.push(Cell::from(Span::styled(
            avg_str,
            Style::default().fg(avg_col),
        )));
        rows.push(Row::new(cells).height(1));
    }

    let sys_fail = app
        .dns
        .cells
        .first()
        .map(|row| row.iter().all(|c| c.last.is_none()))
        .unwrap_or(false);
    let all_fail = app
        .dns
        .cells
        .iter()
        .all(|row| row.iter().all(|c| c.last.is_none()));
    let title_suffix = if all_fail {
        "  ⚠ ALL DNS FAILING"
    } else if sys_fail {
        "  ⚠ SYSTEM DNS DOWN — alternates OK"
    } else {
        ""
    };
    let title = format!(
        "DNS Matrix  {}×{}  (warn {}ms / bad {}ms){}",
        app.dns.resolvers.len(),
        app.dns.names.len(),
        warn as u64,
        bad as u64,
        title_suffix
    );

    let mut widths: Vec<Constraint> = vec![Constraint::Length(10)];
    for _ in 0..app.dns.names.len() {
        widths.push(Constraint::Length(10));
    }
    widths.push(Constraint::Length(10));

    let t = Table::new(rows, widths)
        .header(header)
        .block(header_block(&title));
    f.render_widget(t, area);
}

fn short_name(d: &str) -> &str {
    let trimmed = d.trim_end_matches('.');
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.len() >= 2 {
        parts[parts.len() - 2]
    } else {
        trimmed
    }
}

fn draw_heatmap(f: &mut Frame, app: &App, area: Rect) {
    let n = app.hist.len();
    let title = format!(
        "Heatmap (last {} × 30s, pooled)  peak RTT per bucket, loss also red",
        n
    );

    let block = header_block(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 2 || n == 0 {
        return;
    }
    let cells = app.hist.iter().collect::<Vec<&HistBucket>>();
    let cols = cells.len().min(inner.width as usize);
    let rows = inner.height as usize;
    let mut x = inner.x;
    let cell_w = 1;
    let warn = app.cfg.latency_warn_ms;
    let bad = app.cfg.latency_bad_ms;

    for b in cells.iter().skip(cells.len().saturating_sub(cols)) {
        let (color, fill) = match b.peak_rtt {
            None if b.count == 0 && b.loss == 0 => (MUTED, 0),
            None => (BAD, rows),
            Some(p) => {
                let c = if p > bad {
                    BAD
                } else if p > warn {
                    WARN
                } else if p > warn * 0.6 {
                    ACCENT
                } else {
                    OK
                };
                let ratio = (p / bad).min(1.2) / 1.2;
                (c, ((ratio * rows as f64).ceil() as usize).min(rows))
            }
        };
        let (color, fill) = if b.loss > 0 && color != BAD {
            (BAD, fill.max(1))
        } else {
            (color, fill)
        };

        for r in 0..rows {
            let cell = Rect {
                x,
                y: inner.y + (rows - 1 - r) as u16,
                width: cell_w,
                height: 1,
            };
            let on = r < fill;
            let bg = if on { color } else { DIM };
            f.render_widget(
                ratatui::widgets::Block::default().style(Style::default().bg(bg)),
                cell,
            );
        }
        x += cell_w;
        if x > inner.right() {
            break;
        }
    }
}

fn draw_targets(f: &mut Frame, app: &App, area: Rect) {
    let threshold = app.cfg.degraded_loss_pct;
    let rows: Vec<Row> = app
        .primaries
        .iter()
        .map(|p| {
            let sc = state_color(p.state);
            let lpct = if p.total == 0 {
                0.0
            } else {
                p.lost as f64 * 100.0 / p.total as f64
            };
            let last = p
                .last_value
                .map(|v| format!("{:.0} ms", v))
                .unwrap_or_else(|| "timeout".into());
            let avg = p
                .stat
                .avg()
                .map(|v| format!("{:.0}", v))
                .unwrap_or_else(|| "—".into());
            let dot = match p.state {
                LinkState::Up => "●",
                LinkState::Degraded => "◐",
                LinkState::Down => "○",
            };
            let lc = if lpct >= threshold {
                BAD
            } else if lpct > 0.0 {
                WARN
            } else {
                OK
            };
            let hold = match p.recover_at {
                Some(t) => {
                    let elapsed = t.elapsed().as_secs();
                    let remain = app.cfg.recover_dwell.as_secs().saturating_sub(elapsed);
                    format!("recover in {}s", remain)
                }
                None => String::new(),
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    format!(" {}  {:<6}", dot, p.label),
                    Style::default().fg(sc).add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    format!("{:<15}", p.host),
                    Style::default().fg(Color::Gray),
                )),
                Cell::from(Span::styled(state_label(p.state), Style::default().fg(sc))),
                Cell::from(format!("cur {:<10}", last)),
                Cell::from(format!("avg {:<6}", avg)),
                Cell::from(Span::styled(
                    format!("loss {:>5.1}%  {}/{}", lpct, p.lost, p.total),
                    Style::default().fg(lc),
                )),
                Cell::from(Span::styled(hold, Style::default().fg(MUTED))),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(15),
        Constraint::Length(17),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Min(15),
    ];
    let title = format!("Targets  {} consensus members", app.primaries.len());
    let t = Table::new(rows, widths)
        .header(Row::new(vec![
            Cell::from(Span::styled(" target", Style::default().fg(MUTED))),
            Cell::from(Span::styled(" host", Style::default().fg(MUTED))),
            Cell::from(Span::styled(" state", Style::default().fg(MUTED))),
            Cell::from(Span::styled(" rtt", Style::default().fg(MUTED))),
            Cell::from(Span::styled(" avg", Style::default().fg(MUTED))),
            Cell::from(Span::styled(" loss", Style::default().fg(MUTED))),
            Cell::from(Span::styled("", Style::default().fg(MUTED))),
        ]))
        .block(header_block(&title));
    f.render_widget(t, area);
}

fn draw_extras(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .extras
        .iter()
        .map(|e| {
            let sc = state_color(e.state);
            let lpct = if e.total == 0 {
                0.0
            } else {
                e.lost as f64 * 100.0 / e.total as f64
            };
            let last_str = e
                .last
                .map(|v| format!("{:6.1} ms", v))
                .unwrap_or_else(|| "   ---  ".into());
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:<10}", e.label), Style::default().fg(ACCENT)),
                Span::styled(format!(" {:<12}", e.host), Style::default().fg(Color::Gray)),
                Span::styled(state_label(e.state), Style::default().fg(sc)),
                Span::raw(format!("   cur {}", last_str)),
                Span::styled(
                    format!("  loss {:.1}%  {}/{}", lpct, e.lost, e.total),
                    Style::default().fg(if lpct > 0.0 { BAD } else { OK }),
                ),
            ]))
        })
        .collect();
    f.render_widget(
        List::new(items).block(header_block("Secondary probes")),
        area,
    );
}

fn draw_stats(f: &mut Frame, app: &App, area: Rect) {
    let peak = app.peak_latency;
    let peak_jit = app.peak_jitter;
    let worst_burst = app.worst_loss_burst;
    let best_up_h = app.best_uptime_secs / 3600;
    let best_up_m = (app.best_uptime_secs % 3600) / 60;
    let best_up_s = app.best_uptime_secs % 60;
    let cur_up_h = app.cur_uptime_secs() / 3600;
    let cur_up_m = (app.cur_uptime_secs() % 3600) / 60;
    let cur_up_s = app.cur_uptime_secs() % 60;

    let mttr_s = app.mttr_ms() / 1000;
    let mttr_h = mttr_s / 3600;
    let mttr_m = (mttr_s % 3600) / 60;
    let mttr_sec = mttr_s % 60;
    let up_pct = app.uptime_pct();
    let deg_pct = app.degraded_pct();
    let down_pct = app.down_pct();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" best uptime ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:02}:{:02}:{:02}", best_up_h, best_up_m, best_up_s),
                Style::default().fg(OK),
            ),
            Span::raw("   "),
            Span::styled(" current up ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:02}:{:02}:{:02}", cur_up_h, cur_up_m, cur_up_s),
                Style::default().fg(ACCENT),
            ),
            Span::raw("   "),
            Span::styled(" worst burst ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{}", worst_burst),
                Style::default().fg(if worst_burst > 0 { WARN } else { OK }),
            ),
            Span::raw("   "),
            Span::styled(" peak lat ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:.0} ms", peak),
                Style::default().fg(if peak > app.cfg.latency_bad_ms {
                    BAD
                } else {
                    OK
                }),
            ),
            Span::raw("   "),
            Span::styled(" peak jit ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:.0} ms", peak_jit),
                Style::default().fg(if peak_jit > app.cfg.jitter_warn_ms {
                    WARN
                } else {
                    OK
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled(" up ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:.1}%", up_pct),
                Style::default().fg(if up_pct >= 95.0 { OK } else { WARN }),
            ),
            Span::raw("   "),
            Span::styled(" degraded ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:.1}%", deg_pct),
                Style::default().fg(if deg_pct > 0.0 { WARN } else { OK }),
            ),
            Span::raw("   "),
            Span::styled(" down ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:.1}%", down_pct),
                Style::default().fg(if down_pct > 0.0 { BAD } else { OK }),
            ),
            Span::raw("   "),
            Span::styled(" MTTR ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:02}:{:02}:{:02}", mttr_h, mttr_m, mttr_sec),
                Style::default().fg(ACCENT),
            ),
            Span::raw("   "),
            Span::styled(" recoveries ", Style::default().fg(MUTED)),
            Span::styled(format!("{}", app.recoveries), Style::default().fg(OK)),
            Span::raw("   "),
            Span::styled(" outages ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{}", app.outages),
                Style::default().fg(if app.outages > 0 { BAD } else { OK }),
            ),
        ]),
    ];

    if area.height >= 5
        && area.width >= 100
        && (!app.top_latency.is_empty() || !app.top_jitter.is_empty())
    {
        let lat_spikes: String = app
            .top_latency
            .iter()
            .map(|(v, ts)| format!("{:.0}ms@{}", v, fmt_clock_short(*ts)))
            .collect::<Vec<_>>()
            .join("  ");
        let jit_spikes: String = app
            .top_jitter
            .iter()
            .map(|(v, ts)| format!("{:.0}ms@{}", v, fmt_clock_short(*ts)))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(vec![
            Span::styled(" top lat ", Style::default().fg(MUTED)),
            Span::styled(lat_spikes, Style::default().fg(WARN)),
            Span::raw("   "),
            Span::styled(" top jit ", Style::default().fg(MUTED)),
            Span::styled(jit_spikes, Style::default().fg(WARN)),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            " Streaks ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_events(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .events
        .iter()
        .rev()
        .map(|e| {
            let (c, icon) = match e.level {
                Level::Info => (Color::Gray, "·"),
                Level::Warn => (WARN, "!"),
                Level::Bad => (BAD, "✗"),
                Level::Good => (OK, "✓"),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(c)),
                Span::styled(format!("{} ", fmt_wall(e.ts)), Style::default().fg(MUTED)),
                Span::styled(e.msg.clone(), Style::default().fg(c)),
            ]))
        })
        .collect();
    let title = format!("Events ({}/{}, newest first)", app.events.len(), EVENTS_CAP);
    f.render_widget(List::new(items).block(header_block(&title)), area);
}

fn fmt_wall(t: std::time::SystemTime) -> String {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let ms = dur.subsec_millis();
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (yy, mm, dd) = civil_from_days(days as i64);
    format!(
        "{:02}/{:02}/{:02} {:02}:{:02}:{:02}.{:03}",
        dd,
        mm,
        yy % 100,
        h,
        m,
        s,
        ms
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn fmt_clock_short(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    format!("{:02}:{:02}", h, m)
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let foot = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED));
    let cur_ms = app.interval_ms.load(std::sync::atomic::Ordering::Relaxed);
    let mute_label = if app.muted { "muted" } else { "sound on" };
    let mute_color = if app.muted { MUTED } else { OK };
    let foot_line = vec![
        Span::styled(" [", Style::default().fg(MUTED)),
        Span::styled(
            "m",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("] ", Style::default().fg(MUTED)),
        Span::styled(mute_label, Style::default().fg(mute_color)),
        Span::raw("   "),
        Span::styled("[", Style::default().fg(MUTED)),
        Span::styled(
            "r",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("] reset   ", Style::default().fg(MUTED)),
        Span::styled("[", Style::default().fg(MUTED)),
        Span::styled(
            "e",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("] export   ", Style::default().fg(MUTED)),
        Span::styled("[", Style::default().fg(MUTED)),
        Span::styled(
            "t",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("] traceroute   ", Style::default().fg(MUTED)),
        Span::styled("[", Style::default().fg(MUTED)),
        Span::styled(
            "q",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("] quit", Style::default().fg(MUTED)),
        Span::raw("   "),
        Span::styled(
            format!(
                "{} pings, {} lost  cadence {}ms",
                app.pooled_total(),
                app.pooled_lost(),
                cur_ms
            ),
            Style::default().fg(Color::Gray),
        ),
    ];
    let foot_line = Line::from(foot_line);
    f.render_widget(foot.title(foot_line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_anchors() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(10957), (2000, 1, 1));
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
    }
}
