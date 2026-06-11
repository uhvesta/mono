use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub(crate) const AUTH_ENDPOINT: &str = "oauth2/token/";
pub(crate) const CLIENT_ID: &str = "c82SH0WZOsabOXGP2sxqcj34FxkvfnWRZBKlBjFS";
pub(crate) const EXPIRES_IN_SECONDS: u32 = 86400;
pub(crate) const GRANT_TYPE: &str = "password";
pub(crate) const GR_KEY: &str = "6Leq-w4qAAAAAEAT7gwYSJ21_sYIejmG73oR3fAO";
pub(crate) const GR_TOKEN: &str = "0cAFcWeA69Bfk9ZDZP_jFC2LH-_ZT_c8dwmg7a_elyHXKkZPcnRRkr6NecfEPoEeNyQo9HBMlTFolNtq5wTC1QsNZkdtZhLxLEEjoy_l_aI-iCVcMDeBzUw4oF-40vGjk96IjHzPZjISsdMZyVo8_OKjiWxIYL2epE48K_Z9RkYiiCHFnxiBvOhi5L7uDE8qLwhAS-dJJyVajOOXsNZKGMePLC-pENsWasH9FXuImHrKVxp7GG21qPq91Lu9NuIQv4io6GdZMKAYZGSIgzaeW4QcF6sXMuqa1RTicuXJoKCdjOg6zd5u1IoXk-tpmEHC8vgEd5GSWXXpL5rPNvdguAF9Bj6xxT-If7pZNGJGbihbjVesT_kI-J_7261b_JrNm-m_6N8xpM9nJ5qPljbCVmVXJFYQdSU39iLCcHGDsmlYveV6Phnm-XpVYl-6w-VXjohrs3C6bdkkhGToxKSlpFPlV3xlHif__NR5MyUTTLfT3PIS_-P9kT_zgI_XUJrvYPwoih-pyBvFYSuGYHJP5Dtgyi4_a__wGR1qnBywIJrXhzcQhdSC9W-qMzG_od__DZ47o99RrMMiV3oVTYvO2JWoFmzcCcdkHzXVg7JklkrPsA0JI8wgIUk37Pj3aIYAeaabT6oz5X9nnW2wUzq7qekpfwEPP4xXR28z5HffW-2duNi1X53oGbKB8VY35-Y281lUrk5zz2oGLLxyHTUINfQnU4lloaql1k8Vlt-hRgZy91WlEjeUGdYBaq_t-Kj9-G8sPqt2KoNj3MEtW7uOJhZfnspduyWRL8W1NnurbIwa_ZgPzpLAvr0ZsjTA_EhvSlhg4YlkyrVkCAKK7bMwdqrmkXnxn-vAwved9xcURnl-7finVk93ssoz6upWUQqY5ZVQaWSVudkW6t70de0Y6PpJ9CS224o-A8XvdDde2mP7MesTMeK3ZFPufEGheFk5NKC4AcgjOLJ7SJ1labmGNhJqQWLQYoo59ZtL2oWu2_AFdgFGoF7zo8iRAEgAxsXHyxJyl80ZqWOEARS6u39BBIfVqCZSNnz0GArXn-f8X6yroypbDWJ3bG_sgNFBP_LLP5v9QxPZIl6z6sg4iS_s8_wLiIs2JoMJ2oLlTnOL8UHRp_EDNTSXVlLpKMcNqrLIR3xg2nXGz-5nwdmhDqqewBPHv_GrKlOVuY9pxI42vOkZkBN7cjNFC8Rw94pU7EMPg2yI6fnWj8uf5XRsvOe2Xa3VbvLdQMqgZ2-glXC3n0oZjh4spyuf450dYN8LG1N2uPRs3H4Dd34rkJJdbLUza6SbYaF56tJfewv4VytlGSmV0MnCHLCD18Bq7VAqv6ruTT2JYz6zrd0nEmhOkPPQIUtigkSSEM7asnB_8X9zUWtAxLoq-5L7sNaAH_1-EgeOl_CdmSbSEFfQ55d1V6m9jSVwnwie1EaKWDNGmuA-CLDT2o4KDRVKgBzFtVewhBelAAhl2aPdE5YnSKrvNPce3J4_Rcia9MBHO7Loh0e5b-6pSpXiGKGqPG-1TW10NWFoiN2QBhMD80AP_xrKLhZ4TZpu_r_bOXEgmtQupL4lyTpRa6vmbXy49IBF0rGsVX3mrr5hzmw_HPzCDbyAbtWuxdAKaSq_Y4KgPkPOgWuyahspH87v-_-TNEHnjeFJYrDPLt0csEH3KC52BA0hYgJw6EkwamfQ89Hfi0Cg";
pub(crate) const LONG_SESSION: bool = true;
pub(crate) const READ_ONLY_SECONDARY_TOKEN: bool = true;
pub(crate) const SCOPE: &str = "internal";
pub(crate) const TOKEN_REQUEST_PATH: &str = "/login/";
pub(crate) const IDENTITY_CLIENT_VERSION: &str = "1.0.0";
pub(crate) const IDENTITY_WORKFLOW_ENDPOINT_PREFIX: &str = "idl/v1/workflow/";

#[derive(Debug, Serialize)]
pub(crate) struct TokenRequest<'a> {
    device_token: &'a str,
    client_id: &'a str,
    create_read_only_secondary_token: bool,
    expires_in: u32,
    grant_type: &'a str,
    scope: &'a str,
    token_request_path: &'a str,
    username: &'a str,
    password: &'a str,
    long_session: bool,
    request_id: &'a str,
    gr_key: &'a str,
    gr_token: &'a str,
}

impl<'a> TokenRequest<'a> {
    pub(crate) fn new(username: &'a str, password: &'a str, device_token: &'a str, request_id: &'a str) -> Self {
        Self {
            device_token,
            client_id: CLIENT_ID,
            create_read_only_secondary_token: READ_ONLY_SECONDARY_TOKEN,
            expires_in: EXPIRES_IN_SECONDS,
            grant_type: GRANT_TYPE,
            scope: SCOPE,
            token_request_path: TOKEN_REQUEST_PATH,
            username,
            password,
            long_session: LONG_SESSION,
            request_id,
            gr_key: GR_KEY,
            gr_token: GR_TOKEN,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct VerificationWorkflow {
    pub id: String,
    pub workflow_status: String,
}

#[derive(Clone, Debug)]
pub struct AuthChallenge {
    device_token: Uuid,
    request_id: Uuid,
    verification_workflow: VerificationWorkflow,
}

impl AuthChallenge {
    pub fn new(device_token: Uuid, request_id: Uuid, verification_workflow: VerificationWorkflow) -> Self {
        Self {
            device_token,
            request_id,
            verification_workflow,
        }
    }

    pub fn device_token(&self) -> Uuid {
        self.device_token
    }

    pub fn request_id(&self) -> Uuid {
        self.request_id
    }

    pub fn verification_workflow(&self) -> &VerificationWorkflow {
        &self.verification_workflow
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub verification_workflow: VerificationWorkflow,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkflowEntryPointRequest<'a> {
    #[serde(rename = "clientVersion")]
    client_version: &'a str,
    #[serde(rename = "id")]
    id: &'a str,
    #[serde(rename = "entryPointAction")]
    entry_point_action: EntryPointAction,
}

impl<'a> WorkflowEntryPointRequest<'a> {
    pub(crate) fn new(workflow_id: &'a str) -> Self {
        Self {
            client_version: IDENTITY_CLIENT_VERSION,
            id: workflow_id,
            entry_point_action: EntryPointAction {},
        }
    }
}

#[derive(Debug, Serialize)]
struct EntryPointAction {}

#[derive(Debug, Serialize)]
pub(crate) struct WorkflowProceedRequest<'a> {
    #[serde(rename = "clientVersion")]
    client_version: &'a str,
    #[serde(rename = "screenName")]
    screen_name: &'a str,
    #[serde(rename = "id")]
    id: &'a str,
    #[serde(rename = "deviceApprovalChallengeAction")]
    device_approval_challenge_action: DeviceApprovalChallengeAction,
}

impl<'a> WorkflowProceedRequest<'a> {
    pub(crate) fn new(workflow_id: &'a str) -> Self {
        Self {
            client_version: IDENTITY_CLIENT_VERSION,
            screen_name: "DEVICE_APPROVAL_CHALLENGE",
            id: workflow_id,
            device_approval_challenge_action: DeviceApprovalChallengeAction { proceed: Proceed {} },
        }
    }
}

#[derive(Debug, Serialize)]
struct DeviceApprovalChallengeAction {
    proceed: Proceed,
}

#[derive(Debug, Serialize)]
struct Proceed {}

pub type FinalTokenResponse = Value;

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRouteResponse {
    pub route: WorkflowRoute,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRoute {
    pub replace: Option<WorkflowRouteReplace>,
    pub exit: Option<WorkflowRouteExit>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRouteReplace {
    pub screen: WorkflowScreen,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRouteExit {
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowScreen {
    pub name: String,
    #[serde(rename = "blockId")]
    pub block_id: Option<String>,
    #[serde(rename = "deviceApprovalChallengeScreenParams")]
    pub device_approval_challenge_screen_params: Option<DeviceApprovalChallengeScreenParams>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DeviceApprovalChallengeScreenParams {
    #[serde(rename = "sheriffChallenge")]
    pub sheriff_challenge: Option<SheriffChallenge>,
    #[serde(rename = "sheriffFlowId")]
    pub sheriff_flow_id: Option<String>,
    #[serde(rename = "fallbackCtaText")]
    pub fallback_cta_text: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SheriffChallenge {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub challenge_type: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "remainingRetries")]
    pub remaining_retries: Option<u32>,
    #[serde(rename = "remainingAttempts")]
    pub remaining_attempts: Option<u32>,
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<String>,
}
