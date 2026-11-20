//! A QR is held only while the settings panel displays its invitation.
use std::sync::{Arc, Mutex, Weak};

pub use muqun_gateway::PairingCode as Code;

#[derive(Clone, Debug)]
pub struct Invite {
    pub address: String,
    pub pending: Option<Code>,
    qr: Arc<rune::Pixels>,
}

impl PartialEq for Invite {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
            && self.pending == other.pending
            && Arc::ptr_eq(&self.qr, &other.qr)
    }
}

impl Eq for Invite {}

impl Invite {
    pub fn qr_view(&self) -> rune::El {
        let side = self.qr.size().0 as f32;
        rune::el()
            .key("local-pairing-qr")
            .size(side, side)
            .pixels(&self.qr)
    }
}

pub async fn load() -> anyhow::Result<Invite> {
    static CACHE: Mutex<Option<(String, Weak<rune::Pixels>)>> = Mutex::new(None);
    let invitation = muqun_gateway::local_pairing_invitation().await?;
    let key = invitation.fingerprint();
    let mut cache = CACHE
        .lock()
        .map_err(|_| anyhow::anyhow!("QR cache unavailable"))?;
    let existing = cache
        .as_ref()
        .filter(|(id, _)| *id == key)
        .and_then(|(_, pixels)| pixels.upgrade());
    let qr = if let Some(existing) = existing {
        existing
    } else {
        let (side, rgba) = invitation.qr_rgba()?;
        let pixels = Arc::new(rune::Pixels::new());
        pixels.set(side, side, rgba);
        *cache = Some((key, Arc::downgrade(&pixels)));
        pixels
    };
    Ok(Invite {
        address: invitation.address,
        pending: invitation.pending,
        qr,
    })
}
