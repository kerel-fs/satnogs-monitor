use crate::satnogs::Data;
use crate::sysinfo::SysInfo;
use log::Level;

pub enum Event {
    Input(termion::event::Event),
    Log((Level, String)),
    CommandResponse(Data),
    Resize,
    SystemInfo(Vec<u64>, SysInfo),
    Tick,
    WaterfallCreated(u64, Vec<f32>),
    WaterfallData(f32, Vec<f32>),
    WaterfallClosed(u64),
}
