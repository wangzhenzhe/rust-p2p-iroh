use std::sync::Arc;
use anyhow::Result;
use clap::Parser;
use iroh::PublicKey;
use iroh_base::ticket::NodeTicket;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;
use tracing::{info, warn};
use lazy_static::lazy_static;

use rust_p2p_iroh::p2p::P2PNode;

#[derive(Debug, Clone)]
struct Message {
    from: PublicKey,
    payload: Vec<u8>,
}

#[derive(Debug, Parser)]
struct Args {
    /// 要连接的节点凭证。如果未提供，程序将监听传入连接。
    ticket: Option<NodeTicket>,
}

lazy_static! {
    static ref MESSAGE_TX: broadcast::Sender<Message> = {
        let (tx, _) = broadcast::channel(100);
        tx
    };
}

/// 处理接收到的消息
/// 
/// 参数:
/// - from: 消息发送者的公钥
/// - payload: 消息内容
/// 
/// 将接收到的消息通过广播通道发送给所有监听者
async fn handle_message(from: PublicKey, payload: Vec<u8>) {
    let msg = Message { from, payload };
    if let Err(e) = MESSAGE_TX.send(msg) {
        warn!("广播接收到的消息失败: {:?}", e);
    }
}

/// 程序入口函数
/// 
/// 主要功能:
/// 1. 初始化 P2P 节点
/// 2. 根据命令行参数决定工作模式（监听或连接）
/// 3. 处理用户输入和接收消息
/// 4. 维护节点连接状态
#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志系统
    tracing_subscriber::fmt::init();
    
    info!("正在启动 P2P 聊天程序...");
    
    // 解析命令行参数
    let args = Args::parse();
    
    // 创建P2P节点实例
    let node = Arc::new(P2PNode::new(None).await?);
    info!("节点创建成功ID: {}", node.get_node_id());
    
    // 启动监听服务
    let node_clone = node.clone();
    node.start_listening(move |from, payload| {
        let node = node_clone.clone();
        async move {
            handle_message(from, payload).await;
        }
    }).await?;
    info!("节点开始监听");
    
    if let Some(ticket) = args.ticket {
        // 连接到远程节点
        info!("正在连接到远程节点...");
        let addr = ticket.node_addr().clone();
        let node_clone = node.clone();
        node.connect(addr, move |from, payload| {
            let node = node_clone.clone();
            async move {
                handle_message(from, payload).await;
            }
        }).await?;
        info!("成功连接到远程节点");
    } else {
        // 打印本节点信息
        let addr = node.get_node_addr().await?;
        let ticket = NodeTicket::new(addr);
        println!("\n=== 节点信息 ===");
        println!("节点 ID: {}", node.get_node_id());
        println!("节点凭证: {}", ticket);
        println!("===============\n");
    }
    
    println!("\n=== 聊天就绪 ===");
    println!("输入消息并按回车发送");
    println!("================\n");
    
    // 设置消息接收和输入处理
    let mut rx = MESSAGE_TX.subscribe();
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    
    // 启动连接状态检查任务
    let node_clone = node.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            if let Err(e) = node_clone.cleanup_dead_connections().await {
                warn!("清理断开连接时出错: {:?}", e);
            }
        }
    });

    loop {
        tokio::select! {
            // 处理接收到的消息
            Ok(msg) = rx.recv() => {
                if String::from_utf8_lossy(&msg.payload) == "CONNECTED\n" {
                    info!("收到来自节点 {} 的连接确认", msg.from);
                    continue;
                }
                println!("{}> {}", msg.from, String::from_utf8_lossy(&msg.payload));
            }
            
            // 处理用户输入
            Ok(Some(line)) = reader.next_line() => {
                let peers = node.get_connections().await;
                if peers.is_empty() {
                    println!("警告：没有已连接的节点");
                    continue;
                }
                
                for peer in peers {
                    match node.send_message(peer, line.as_bytes().to_vec()).await {
                        Ok(_) => info!("消息已发送至节点 {}", peer),
                        Err(e) => warn!("发送消息至节点 {} 失败: {:?}", peer, e),
                    }
                }
            }
        }
    }
} 