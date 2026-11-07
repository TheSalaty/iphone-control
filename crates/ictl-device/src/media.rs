//! 启动 CoreDevice 屏幕媒体流。HID 事件只有在媒体流存在时才会被系统接受（否则静默丢弃），
//! 所以 HID 会话期间要一直持有它；RTP 数据不读，直接丢弃。
//!
//! 移植自 idevice v0.1.68 `tools/src/coredevice_stream.rs`（MIT，Copyright Jackson Coxson），
//! 去掉了日志输出和应答转储，错误改成 `DeviceError`。

use idevice::{
    ReadWrite, RsdService,
    core_device::{
        CallInfoBlob, DisplayServiceClient, build_screen_audio_offer, build_screen_video_offer,
        build_start_audio_parameters, build_start_video_parameters,
    },
    rsd::RsdHandshake,
    tcp::handle::{AdapterHandle, UdpSocketHandle},
};

use std::time::Duration;

use crate::{DeviceError, Result, limit};

/// 每一步的时限。超时不能在外层直接取消：音频流可能已经起来了，必须走到 stop
const STEP: Duration = Duration::from_secs(10);

/// 我们（控制端）支持的特性，不是设备的。Device Hub 屏幕共享发 140；
/// 发设备的掩码会让协商出无效的视频配置。
const CLIENT_SUPPORTED_FEATURES: u64 = 140;

/// 活着的媒体流。两个 UDP 套接字必须一直绑定，丢掉就会解绑端口。
pub(crate) struct ScreenMediaStream {
    client: DisplayServiceClient<Box<dyn ReadWrite>>,
    _audio_udp: UdpSocketHandle,
    _video_udp: UdpSocketHandle,
}

impl ScreenMediaStream {
    pub async fn stop(mut self) {
        stop(&mut self.client).await;
    }
}

pub(crate) async fn start(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    display_id: i64,
) -> Result<ScreenMediaStream> {
    let fail = |what: &str, e: &dyn std::fmt::Display| {
        DeviceError::Idevice(ictl_l10n::trf!("媒体流 {what}：{e}", "media stream {what}: {e}"))
    };

    let mut client = limit(ictl_l10n::tr!("连接 displayservice", "connect to displayservice"), STEP, async {
        DisplayServiceClient::connect_rsd(adapter, handshake)
            .await
            .map_err(|e| fail(ictl_l10n::tr!("连接 displayservice", "connect to displayservice"), &e))
    })
    .await?;
    let audio_udp =
        adapter.bind_udp(0).await.map_err(|e| fail(ictl_l10n::tr!("绑定音频 UDP", "bind audio UDP"), &e))?;
    let video_udp =
        adapter.bind_udp(0).await.map_err(|e| fail(ictl_l10n::tr!("绑定视频 UDP", "bind video UDP"), &e))?;
    let receiver_ip = adapter.host_ip().to_string();
    let sender_ip = adapter.peer_ip().to_string();

    // 描述本端的 VCCallInfoBlob，取值与上游抓到的、设备接受的 Device Hub 报价一致
    let call_info = CallInfoBlob {
        call_id: 0,
        client_version: 1,
        device_type: "Mac17,7".into(),
        framework_version: "2205.3.1".into(),
        os_version: "25F71".into(),
        device_name: None,
        audio_device_uid: None,
    };
    let session = uuid::Uuid::new_v4();

    // 先起音频流建立屏幕共享会话，再在同一会话上起视频流
    let audio_call_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let audio_offer = build_screen_audio_offer(&audio_call_id, &call_info)
        .map_err(|e| fail(ictl_l10n::tr!("音频报价", "audio offer"), &e))?;
    let audio_params = build_start_audio_parameters(
        &receiver_ip,
        audio_udp.local_port(),
        &sender_ip,
        50000,
        audio_offer,
        CLIENT_SUPPORTED_FEATURES,
        session,
    );

    let call_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let offer = build_screen_video_offer(&call_id, &call_info, ssrc)
        .map_err(|e| fail(ictl_l10n::tr!("视频报价", "video offer"), &e))?;
    let params = build_start_video_parameters(
        &receiver_ip,
        video_udp.local_port(),
        &sender_ip,
        50001,
        offer,
        CLIENT_SUPPORTED_FEATURES,
        display_id,
        session,
    );
    let started = limit(ictl_l10n::tr!("启动媒体流", "start the media stream"), STEP * 2, async {
        client
            .start_media_stream(audio_params)
            .await
            .map_err(|e| fail(ictl_l10n::tr!("启动音频流", "start the audio stream"), &e))?;
        client
            .start_media_stream(params)
            .await
            .map_err(|e| fail(ictl_l10n::tr!("启动视频流", "start the video stream"), &e))
    })
    .await;
    if let Err(e) = started {
        // 音频流可能已经起来了（包括超时的情况），要停掉：stopAll 连同会话一起结束，否则手机端一直挂着
        stop(&mut client).await;
        return Err(e);
    }

    Ok(ScreenMediaStream { client, _audio_udp: audio_udp, _video_udp: video_udp })
}

async fn stop(client: &mut DisplayServiceClient<Box<dyn ReadWrite>>) {
    match tokio::time::timeout(STEP, client.stop_media_stream()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::debug!("stop_media_stream: {e}"),
        Err(_) => tracing::warn!(
            "{}",
            ictl_l10n::tr!(
                "停止屏幕媒体流超时，手机上可能还挂着屏幕共享会话",
                "Stopping the screen media stream timed out; a screen sharing session may still be open on the phone"
            )
        ),
    }
}
