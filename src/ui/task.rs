//! Sole screen writer, on its own executor thread.
use crate::gateway::bus;
use crate::ui::Ui;
use crate::ui::view::ViewState;
use embassy_time::{Duration, Ticker};

fn lower_own_priority() {
    use esp_idf_svc::sys;
    unsafe {
        let handle = sys::xTaskGetCurrentTaskHandle();
        sys::vTaskPrioritySet(handle, 1);
    }
}

#[embassy_executor::task]
pub async fn ui_task(mut ui: Ui) {
    lower_own_priority();
    let mut state = ViewState::default();
    let mut ticker = Ticker::every(Duration::from_secs(1));
    loop {
        state.apply(bus::ui_snapshot());
        state.render(&mut ui);
        ticker.next().await;
    }
}
