use crate::models::ProcessInfo;
use crate::models::ProcessStatus;
use std::{
    collections::HashMap,
    mem::size_of,
    sync::{Arc, RwLock},
    time::Duration,
};
use memmap2::MmapMut;
use tokio::sync::mpsc;

pub struct StatusMonitor {
    shared_mem: Arc<RwLock<MmapMut>>,
    process_map: Arc<RwLock<HashMap<u64, usize>>>,
    output_tx: mpsc::Sender<String>,
}

impl StatusMonitor {
    pub fn new(
        shared_mem: Arc<RwLock<MmapMut>>,
        process_map: Arc<RwLock<HashMap<u64, usize>>>,
    ) -> (Self, mpsc::Receiver<String>) {
        let (output_tx, output_rx) = mpsc::channel(1024);
        (
            Self {
                shared_mem,
                process_map,
                output_tx,
            },
            output_rx,
        )
    }

    pub async fn run(&self) {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        let mut last_status: HashMap<u64, (ProcessStatus, i32)> = HashMap::new();
    
        loop {
            interval.tick().await;
            
            // Collect all nonces and offsets first
            let entries: Vec<(u64, usize)> = {
                let map = self.process_map.read().unwrap();
                map.iter().map(|(&k, &v)| (k, v)).collect()
            };
    
            for (nonce, offset) in entries {
                let status_str = {
                    let mmap = self.shared_mem.read().unwrap();
                    let info_slice = &mmap[offset..offset + size_of::<ProcessInfo>()];
                    let info = unsafe { std::ptr::read(info_slice.as_ptr() as *const ProcessInfo) };
                    
                    let status_key = (info.status, info.exit_code);
                    if last_status.get(&nonce) != Some(&status_key) {
                        last_status.insert(nonce, status_key);
                        format!("{}{}{}", nonce, info.status as u8 as char, info.exit_code)
                    } else {
                        continue;
                    }
                };
    
                if let Err(e) = self.output_tx.send(status_str).await {
                    eprintln!("Failed to send status update: {}", e);
                }
            }
        }
    }
}
