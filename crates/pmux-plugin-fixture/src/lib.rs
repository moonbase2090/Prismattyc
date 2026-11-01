// SPDX-License-Identifier: MIT
//! Real Wasm guest used by runtime tests; it is not a distributed plug-in.

#[cfg(not(target_arch = "wasm32"))]
mod build;
#[cfg(not(target_arch = "wasm32"))]
pub use build::{component, module};

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn fixture_start() {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "wasm32")]
mod guest {
    use std::cell::{Cell, RefCell};

    use pmux_plugin_api::guest::pmux::{
        activity::read, plugin::lifecycle, tool::call, view::frame,
    };

    thread_local! {
        static VIEW: RefCell<Option<lifecycle::ViewReference>> = const { RefCell::new(None) };
        static COUNTER: Cell<u32> = const { Cell::new(0) };
    }

    struct Fixture;

    impl pmux_plugin_api::guest::Guest for Fixture {
        fn handle(event: lifecycle::Message) -> Result<lifecycle::Reply, String> {
            match event {
                lifecycle::Message::Start(context) => {
                    if !matches!(read::endpoints(), Err(read::Error::NotActivated)) {
                        return Err("preparation admitted data access".into());
                    }
                    if pmux_plugin_api::guest::pmux::plugin::host::asset("fixture-fail-ready")
                        .is_ok()
                    {
                        return Err("fixture rejected readiness".into());
                    }
                    if let lifecycle::Role::View(view) = context.role {
                        VIEW.with(|state| *state.borrow_mut() = Some(view));
                    }
                    Ok(lifecycle::Reply::Ready(context.api_version))
                }
                lifecycle::Message::Tool(request) => {
                    if request.contribution == "counter" {
                        let value = COUNTER.with(|counter| {
                            let next = counter.get() + 1;
                            counter.set(next);
                            next
                        });
                        return response(request.id, serde_json::json!({"counter":value}), false);
                    }
                    if request.contribution == "loop" {
                        loop {
                            core::hint::spin_loop();
                        }
                    }
                    if request.contribution == "grow" {
                        let mut bytes = Vec::<u8>::new();
                        let denied = bytes.try_reserve_exact(64 * 1024 * 1024).is_err();
                        return response(
                            request.id,
                            serde_json::json!({"allocation_denied": denied}),
                            false,
                        );
                    }
                    let result = read::endpoints().and_then(|endpoints| {
                        let first = endpoints.first().ok_or(read::Error::UnknownEndpoint)?;
                        read::poll(first.handle, None, 256)
                    });
                    match result {
                        Ok(page) => {
                            let seat = page
                                .records
                                .first()
                                .map(|record| record.event.seat.as_str());
                            response(
                                request.id,
                                serde_json::json!({
                                    "epoch": page.epoch, "sequence": page.current_sequence, "seat": seat,
                                }),
                                false,
                            )
                        }
                        Err(error) => {
                            response(request.id, serde_json::json!({"error": error.name()}), true)
                        }
                    }
                }
                lifecycle::Message::View(event) => VIEW.with(|state| {
                    let mut state = state.borrow_mut();
                    let view = state.as_mut().ok_or("view was not prepared")?;
                    if let frame::Event::Resize(viewport) = &event.event {
                        view.viewport = viewport.clone();
                    }
                    let malicious = matches!(
                        event.event,
                        frame::Event::Tick(frame::Clock { wall_ms: 666, .. })
                    );
                    let count =
                        usize::from(view.viewport.columns) * usize::from(view.viewport.rows);
                    let mut cells: Vec<_> = (0..count).map(|_| cell(" ")).collect();
                    if let Some(first) = cells.first_mut() {
                        first.text = if malicious {
                            "\x1b]52;c;payload\x07"
                        } else {
                            "F"
                        }
                        .into();
                    }
                    Ok(lifecycle::Reply::Frame(lifecycle::ViewFrame {
                        id: event.id,
                        content: frame::Content {
                            columns: view.viewport.columns,
                            rows: view.viewport.rows,
                            cells,
                            tiles: vec![],
                            cursor: None,
                        },
                    }))
                }),
                lifecycle::Message::Stop => {
                    if !matches!(read::endpoints(), Err(read::Error::Revoked)) {
                        return Err("stop retained data authority".into());
                    }
                    Ok(lifecycle::Reply::Idle)
                }
            }
        }
    }

    fn response(
        id: u64,
        value: serde_json::Value,
        is_error: bool,
    ) -> Result<lifecycle::Reply, String> {
        Ok(lifecycle::Reply::Tool(call::Response {
            id,
            json: value.to_string(),
            is_error,
        }))
    }

    fn cell(text: &str) -> frame::Cell {
        frame::Cell {
            text: text.into(),
            width: 1,
            foreground: frame::Color::Default,
            background: frame::Color::Default,
            attributes: frame::Attributes {
                bold: false,
                dim: false,
                italic: false,
                blink: false,
                reverse: false,
                hidden: false,
                strike: false,
                overline: false,
            },
            underline: frame::Underline::None,
        }
    }

    pmux_plugin_api::guest::export_plugin!(Fixture);
}
