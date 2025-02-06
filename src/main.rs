use anyhow::Result;
use clap::Parser;
use iroh_base::ticket::NodeTicket;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

use rust_p2p_iroh::p2p::P2PNode;

#[derive(Debug, Parser)]
struct Args {
    /// 要连接的节点ticket。如果不提供，程序将监听传入连接。
    ticket: Option<NodeTicket>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();
    
    info!("启动P2P聊天程序...");
    
    // 解析命令行参数
    let args = Args::parse();
    
    // 创建一个新的P2P节点
    let node = P2PNode::new(None).await?;
    info!("节点创建成功，ID: {}", node.get_node_id());
    
    // 启动监听
    node.start_listening().await?;
    info!("节点监听已启动");
    
    if let Some(ticket) = args.ticket {
        // 如果提供了ticket，连接到远程节点
        info!("正在连接到远程节点...");
        let addr = ticket.node_addr().clone();
        node.connect(addr).await?;
        info!("已成功连接到远程节点");
    } else {
        // 如果没有提供ticket，打印本节点的ticket
        let addr = node.get_node_addr().await?;
        let ticket = NodeTicket::new(addr);
        println!("\n=== 节点信息 ===");
        println!("节点ID: {}", node.get_node_id());
        println!("节点ticket: {}", ticket);
        println!("===============\n");
    }
    
    // 获取消息接收channel
    info!("设置消息接收处理...");
    let mut rx = node.receive_messages();
    
    // 在后台任务中处理接收到的消息
    tokio::spawn({
        async move {
            info!("开始监听接收消息");
            while let Ok(msg) = rx.recv().await {
                if String::from_utf8_lossy(&msg.payload) == "CONNECTED\n" {
                    info!("收到节点 {} 的连接确认", msg.from);
                    continue;
                }
                println!("{}> {}", msg.from, String::from_utf8_lossy(&msg.payload));
            }
            warn!("消息接收通道已关闭");
        }
    });
    
    info!("开始处理用户输入...");
    // 从标准输入读取消息并发送
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    
    println!("\n=== 聊天已就绪 ===");
    println!("输入消息并按回车发送");
    println!("================\n");
    
    while let Some(line) = reader.next_line().await? {
        // 广播消息给所有连接的节点
        let peers = node.get_connections().await;
        if peers.is_empty() {
            println!("警告: 当前没有连接的节点");
            continue;
        }
        
        for peer in peers {
            match node.send_message(peer, line.as_bytes().to_vec()).await {
                Ok(_) => info!("消息已发送到节点 {}", peer),
                Err(e) => warn!("发送消息到节点 {} 失败: {:?}", peer, e),
            }
        }
    }
    
    // 启动连接状态检查任务
    let node_for_check = node.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            if let Err(e) = node_for_check.cleanup_dead_connections().await {
                warn!("清理断开连接时出错: {:?}", e);
            }
        }
    });
    
    info!("程序结束");
    Ok(())
} 