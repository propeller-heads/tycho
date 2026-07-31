use actix_web::{HttpResponse, Responder};

/// Returns a heap profile in gzipped pprof protobuf format.
///
/// Requires: started with
/// `_RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19`.
///
/// Usage:
///   curl -H "Authorization: $AUTH_API_KEY" -o heap.pb.gz localhost:4242/debug/pprof/heap
///   go tool pprof -http=:8080 heap.pb.gz
#[cfg(feature = "jemalloc")]
pub async fn heap_profile() -> impl Responder {
    let Some(ctl) = jemalloc_pprof::PROF_CTL.as_ref() else {
        return HttpResponse::InternalServerError().body(
            "heap profiling not available — set _RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19",
        );
    };
    let mut prof_ctl = ctl.lock().await;
    if !prof_ctl.activated() {
        return HttpResponse::Forbidden().body("heap profiling not activated");
    }
    match prof_ctl.dump_pprof() {
        Ok(pprof) => HttpResponse::Ok()
            .content_type("application/octet-stream")
            .append_header(("Content-Disposition", "attachment; filename=\"heap.pb.gz\""))
            .body(pprof),
        Err(err) => HttpResponse::InternalServerError().body(format!("heap dump failed: {err}")),
    }
}
