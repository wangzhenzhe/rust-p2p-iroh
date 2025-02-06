# Rust P2P on Iroh

A peer to peer library over Iroh network
This is a simple chat application using the Iroh library.
Also an example of how to use an library created with the Iroh library.

## Getting started

Install Rust toolchain.

## Usage

### Peer to Peer Protocol example

To run the file transfer example, use the following command:

```cmd

cargo run
```

Then the ticket to connect to this application will be printed to the console. Use this ticket on another terminal or device to establish a connection.

```cmd

cargo run <ticket>
```

The connection should be established successfully and "hello" message will show as handshake message. After that, the chat can start.
