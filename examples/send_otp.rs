//! Send a single OTP.
//!
//! MDS_BASE_URL is optional and defaults to https://mdssend.in/api.php.
//!
//!   MDS_USERNAME=123456 MDS_API_KEY=... MDS_SENDER_ID=SENDER \
//!   MDS_COUNTRY_CODE=91 \
//!   cargo run --example send_otp -- 9995551212 482913

use mdsmedia::{MdsClient, Template};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let to = args.next().ok_or("usage: send_otp <number> <otp>")?;
    let otp = args.next().ok_or("usage: send_otp <number> <otp>")?;

    let client = MdsClient::from_env()?;
    let tpl = Template::new("{#var#} is your verification code. Do not share it with anyone.");

    match client.send_otp(&to, &otp, &tpl).await {
        Ok(r) => println!(
            "accepted: id={:?} status={} attempts={} in {:?}",
            r.message_id, r.status, r.attempts, r.elapsed
        ),
        Err(e) => {
            eprintln!("send failed: {e}");
            if e.is_fatal() {
                eprintln!("this is a configuration problem — retrying will not help");
            }
            std::process::exit(1);
        }
    }
    Ok(())
}
