use crate::{
    packet::{DecodeError, NetPacket, PacketData},
    session::Session,
    AppState,
};
use common::config::ServiceType;
use evelyn_encryption::rsa;
use evelyn_proto::*;
use qwer_rpc::{
    middleware::{AccountMiddlewareModel, MiddlewareModel},
    RpcCallError,
};
use rand::RngCore;
use std::time::Duration;
use tracing::{debug, error};

#[derive(thiserror::Error, Debug)]
pub enum PacketHandlingError {
    #[error("decode error: {0}")]
    Decode(#[from] DecodeError),
    #[error("rpc call error: {0}")]
    RpcCallError(#[from] RpcCallError),
}

pub async fn decode_and_handle(
    session: &crate::session::Session,
    state: &'static AppState,
    buf: &[u8],
) -> Result<(), PacketHandlingError> {
    let cmd_id = buf.get_cmd_id()?;

    tracing::debug!("received cmd_id: {cmd_id}");
    match cmd_id {
        PlayerGetTokenCsReq::CMD_ID => {
            let packet = NetPacket::<PlayerGetTokenCsReq>::decode(buf)?;
            on_player_get_token_cs_req(session, state, packet.head, packet.body).await;
        }
        cmd_id if session.is_logged_in() => {
            let middleware_list = vec![MiddlewareModel::Account(AccountMiddlewareModel {
                player_uid: session.get_player_uid() as u64,
                client_protocol_uid: 1,
                is_resend: false,
            })];

            let end_point = session.game_server_addr();
            decode_and_forward_proto!(
                cmd_id,
                buf,
                session,
                session.rpc_ptc_point.lock().await,
                end_point,
                middleware_list,
                Duration::from_secs(2)
            )
        }
        cmd_id => debug!("received cmd_id: {cmd_id}, session is not logged in, expected PlayerGetTokenCsReq (cmd_id: {})", PlayerGetTokenCsReq::CMD_ID),
    }

    Ok(())
}

async fn on_player_get_token_cs_req(
    session: &Session,
    state: &'static AppState,
    head: PacketHead,
    req: PlayerGetTokenCsReq,
) {
    if session.is_logged_in() {
        debug!(
            "received PlayerGetTokenCsReq but session is already logged in! account_uid: {}",
            req.account_uid
        );
        session.send_rsp(
            head.packet_id,
            PlayerGetTokenScRsp {
                retcode: 1008,
                ..Default::default()
            },
        );
        return;
    }

    let conf = &state.remote_config.encryption_conf;
    let client_rand_key = u64::from_le_bytes(
        rsa::decrypt(conf, &rbase64::decode(&req.client_rand_key).unwrap())
            .try_into()
            .unwrap(),
    );

    let server_rand_key = rand::thread_rng().next_u64();
    session.set_secret_key(server_rand_key ^ client_rand_key);

    let server_rand_key = server_rand_key.to_le_bytes();

    let (retcode, uid) = match state
        .db_context
        .get_or_create_uid(&req.account_uid, &req.token)
        .await
    {
        Ok(Some(uid)) => {
            session.set_player_uid(uid);
            (0, uid)
        }
        Ok(None) => (1007, 0), // token mismatch
        Err(err) => {
            error!("get_or_create_uid failed: {err}");
            (1, 0)
        }
    };

    // TODO: multiple game servers, choose random one by load balance manager
    session.bind_game_server(
        state
            .environment
            .get_server_end_point(ServiceType::GameServer, 0)
            .unwrap(),
    );

    session.send_rsp(
        head.packet_id,
        PlayerGetTokenScRsp {
            retcode,
            uid,
            server_rand_key: rbase64::encode(&rsa::encrypt(conf, &server_rand_key)),
            sign: rbase64::encode(&rsa::sign(conf, &server_rand_key)),
            ..Default::default()
        },
    );
}
