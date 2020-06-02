/*!
 * HTTP entrypoint functions for the server controller's exposed API
 */

use crate::api_model::ApiDiskRuntimeState;
use crate::api_model::ApiInstanceRuntimeState;
use crate::api_model::DiskEnsureBody;
use crate::api_model::InstanceEnsureBody;
use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::ExtractedParameter;
use dropshot::HttpError;
use dropshot::HttpResponseOkObject;
use dropshot::HttpResponseUpdatedNoContent;
use dropshot::Json;
use dropshot::Path;
use dropshot::RequestContext;
use http::Method;
use serde::Deserialize;
use std::any::Any;
use std::sync::Arc;
use uuid::Uuid;

use super::ServerController;

pub fn sc_api() -> ApiDescription {
    let mut api = ApiDescription::new();
    if let Err(err) = register_endpoints(&mut api) {
        panic!("failed to register entrypoints: {}", err);
    }
    api
}

fn register_endpoints(api: &mut ApiDescription) -> Result<(), String> {
    api.register(scapi_instance_put)?;
    api.register(scapi_instance_poke_post)?;
    api.register(scapi_disk_put)?;
    api.register(scapi_disk_poke_post)?;
    Ok(())
}

/* TODO-cleanup commonize with ApiContext::from_private? */
fn rqctx_to_sc(rqctx: &Arc<RequestContext>) -> Arc<ServerController> {
    let ctx: Arc<dyn Any + Send + Sync + 'static> =
        Arc::clone(&rqctx.server.private);
    ctx.downcast::<ServerController>().expect("wrong type for private data")
}

#[derive(Deserialize, ExtractedParameter)]
struct InstancePathParam {
    instance_id: Uuid,
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}",
}]
async fn scapi_instance_put(
    rqctx: Arc<RequestContext>,
    path_params: Path<InstancePathParam>,
    body: Json<InstanceEnsureBody>,
) -> Result<HttpResponseOkObject<ApiInstanceRuntimeState>, HttpError> {
    let sc = rqctx_to_sc(&rqctx);
    let instance_id = path_params.into_inner().instance_id;
    let body_args = body.into_inner();
    Ok(HttpResponseOkObject(
        sc.instance_ensure(
            instance_id,
            body_args.initial_runtime.clone(),
            body_args.target.clone(),
        )
        .await?,
    ))
}

#[endpoint {
    method = POST,
    path = "/instances/{instance_id}/poke",
}]
async fn scapi_instance_poke_post(
    rqctx: Arc<RequestContext>,
    path_params: Path<InstancePathParam>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sc = rqctx_to_sc(&rqctx);
    let instance_id = path_params.into_inner().instance_id;
    sc.instance_poke(instance_id).await;
    Ok(HttpResponseUpdatedNoContent())
}

#[derive(Deserialize, ExtractedParameter)]
struct DiskPathParam {
    disk_id: Uuid,
}

#[endpoint {
    method = PUT,
    path = "/disks/{disk_id}",
}]
async fn scapi_disk_put(
    rqctx: Arc<RequestContext>,
    path_params: Path<DiskPathParam>,
    body: Json<DiskEnsureBody>,
) -> Result<HttpResponseOkObject<ApiDiskRuntimeState>, HttpError> {
    let sc = rqctx_to_sc(&rqctx);
    let disk_id = path_params.into_inner().disk_id;
    let body_args = body.into_inner();
    Ok(HttpResponseOkObject(
        sc.disk_ensure(
            disk_id,
            body_args.initial_runtime.clone(),
            body_args.target.clone(),
        )
        .await?,
    ))
}

#[endpoint {
    method = POST,
    path = "/disks/{disk_id}/poke",
}]
async fn scapi_disk_poke_post(
    rqctx: Arc<RequestContext>,
    path_params: Path<DiskPathParam>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sc = rqctx_to_sc(&rqctx);
    let disk_id = path_params.into_inner().disk_id;
    sc.disk_poke(disk_id).await;
    Ok(HttpResponseUpdatedNoContent())
}