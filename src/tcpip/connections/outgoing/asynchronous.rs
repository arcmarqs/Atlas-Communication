use std::sync::Arc;
use std::time::Instant;
use futures::AsyncWriteExt;

use log::error;
use log::warn;

use atlas_common::async_runtime as rt;
use atlas_common::error::*;
use atlas_common::socket::{SecureWriteHalfAsync};
use atlas_metrics::metrics::metric_duration;
use crate::metric::{COMM_REQUEST_SEND_TIME_ID, COMM_RQ_SEND_CLI_PASSING_TIME_ID, COMM_RQ_SEND_PASSING_TIME_ID, COMM_RQ_TIME_SPENT_IN_MOD_ID};
use crate::reconfiguration_node::NetworkInformationProvider;
use crate::serialize::Serializable;

use crate::tcpip::connections::{ConnHandle, NetworkSerializedMessage, PeerConnection, PeerConnections};

pub(super) fn spawn_outgoing_task<NI, RM, PM>(
    conn_handle: ConnHandle,
    node_conns: Arc<PeerConnections<NI, RM, PM>>,
    peer: Arc<PeerConnection<RM, PM>>,
    mut socket: SecureWriteHalfAsync)
    where NI: NetworkInformationProvider + 'static,
          RM: Serializable + 'static,
          PM: Serializable + 'static {
    rt::spawn(async move {
        let mut rx = peer.to_send_handle().clone();

        loop {
            let to_send = match rx.recv_async().await {
                Ok(message) => { message }
                Err(error_kind) => {
                    error!("{:?} // Failed to receive message to send. {:?}", conn_handle.my_id, error_kind);

                    break;
                }
            };

            let dispatch_time = &to_send.2;

            if peer.peer_node_id.id() < 1000 {
                metric_duration(COMM_RQ_SEND_PASSING_TIME_ID, dispatch_time.elapsed());
            } else {
                metric_duration(COMM_RQ_SEND_CLI_PASSING_TIME_ID, dispatch_time.elapsed());
            }

            // If the connection has received an error, disconnect this TX part
            // (As it might have been stuck waiting for a message, and now it
            // would just get an error while trying to write)
            if conn_handle.is_cancelled() {
                warn!("{:?} // Conn {} has been cancelled, returning message to queue", conn_handle.my_id, conn_handle.id);

                // Put the taken request back into the send queue
                if let Err(err) = peer.peer_msg_return_async(to_send).await {
                    error!("{:?} // Failed to return message because {:?}",conn_handle.my_id, err);
                }

                // Return as we don't want to call delete connection again
                return;
            }

            if let Err(_) = send_message(&peer, &mut socket, &conn_handle, to_send, false).await {
                break;
            }

            // Attempt to send all pending messages from the queue in order to avoid doing many sys calls
            while let Ok(to_send) = rx.try_recv() {
                if let Err(_) = send_message(&peer, &mut socket, &conn_handle, to_send, false).await {
                    break;
                }
            }

            // Only flush when there are no more messages to send
            if let Err(_) = socket.flush().await {
                break;
            }
        }

        let remaining_conns = peer.delete_connection(conn_handle.id());

        node_conns.handle_conn_lost(&peer.peer_node_id, remaining_conns);
    });
}

async fn send_message<RM, PM>(peer: &Arc<PeerConnection<RM, PM>>,
                              socket: &mut SecureWriteHalfAsync,
                              conn_handle: &ConnHandle,
                              to_send: NetworkSerializedMessage,
                              flush: bool) -> Result<()>
    where RM: Serializable + 'static,
          PM: Serializable + 'static {
    let start = Instant::now();

    let (to_send, callback, dispatch_time, _, send_rq_time) = to_send;

    match to_send.write_to(socket, flush).await {
        Ok(_) => {
            if let Some(callback) = callback {
                callback(true);
            }

            metric_duration(COMM_REQUEST_SEND_TIME_ID, start.elapsed());

            metric_duration(COMM_RQ_TIME_SPENT_IN_MOD_ID, send_rq_time.elapsed());

            Ok(())
        }
        Err(error_kind) => {
            error!("{:?} // Failed to write message to socket. {:?}", conn_handle.my_id, error_kind);

            // Put the taken request back into the send queue
            if let Err(err) = peer.peer_msg_return_async((to_send, callback, dispatch_time, flush, send_rq_time)).await {
                error!("{:?} // Failed to return message because {:?}", conn_handle.my_id, err);
            }

            Err(Error::simple(ErrorKind::Communication))
        }
    }
}

