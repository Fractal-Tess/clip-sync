use anyhow::Context;

use crate::{
    config::AppPaths,
    crypto::MeshSecret,
    envelope::{RekeyOutcome, rekey_state},
};

use super::commands::RekeyArgs;

pub(super) fn rekey_command(paths: &AppPaths, args: &RekeyArgs) -> anyhow::Result<()> {
    let old_secret =
        MeshSecret::load(&args.old_key_file).context("load old mesh-secret key file")?;
    let new_secret =
        MeshSecret::load(&args.new_key_file).context("load new mesh-secret key file")?;
    match rekey_state(&paths.state_dir, &old_secret, &new_secret)
        .context("rotate encrypted local store keyslot")?
    {
        RekeyOutcome::Rotated => {
            println!("local encrypted store keyslot rotated and verified");
        }
        RekeyOutcome::AlreadyCurrent => {
            println!("local encrypted store keyslot already uses the new secret");
        }
    }
    Ok(())
}
