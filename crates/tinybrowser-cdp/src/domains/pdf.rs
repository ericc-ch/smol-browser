use serde_json::Value;

use crate::dispatch::CdpContext;

pub async fn print_to_pdf(
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    let _ = (params, ctx, session_id);
    Err(crate::domains::page::paint_unsupported("printToPDF"))
}
