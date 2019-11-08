#![allow(unused)]
use async_std::{prelude::*, io::Read, future::try_join};
use std::pin::Pin;
use log::*;

/// Diff two files
pub async fn diff(mut older: Pin<&mut dyn Read>, mut newer: Pin<&mut dyn Read>) -> Result<(), async_std::io::Error>
{
    // let mut obuf = Vec::new();
    // let mut nbuf = Vec::new();

    // {
    //     let a = older.read_to_end(&mut obuf);
    //     let b = newer.read_to_end(&mut nbuf);
    //     try_join!(a, b).await?;
    // }

    // info!("older is {} bytes", obuf.len());
    // info!("newer is {} bytes", nbuf.len());

    Ok(())
}
