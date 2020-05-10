use anyhow::{anyhow, Result};
use cmsis_pack::{pack_index::Vidx, utils::FromElem};

/// Fetches the master VIDX/PIDX file from the ARM server and returns the parsed file.
pub(crate) fn get_vidx() -> Result<Vidx> {
    let reader = reqwest::blocking::Client::new()
        .get("https://www.keil.com/pack/index.pidx")
        .send()?
        .text()?;

    let vidx = match Vidx::from_string(&reader) {
        Ok(vidx) => vidx,
        Err(e) => return Err(anyhow!(e.to_string())),
    };

    Ok(vidx)
}
