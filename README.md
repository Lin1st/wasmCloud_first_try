# In-Host Invocation Optimization for wasmCloud

This project implements a low-latency, in-memory invocation mechanism for co-located components running inside the [wasmCloud](https://wasmcloud.dev) host runtime. The implementation is against **wasmCloud v1.5.0**. The enhancement bypasses NATS-based messaging when possible, significantly improving performance for local component communication while preserving compatibility with wasmCloud's distributed messaging model.

## Problem Statement

The default wasmCloud communication model uses the NATS message broker for all component-to-component invocations — even when components are running on the same host. This introduces unnecessary serialization and routing overhead in cases where direct in-process communication would be more efficient.

## Implementation Overview

The implementation introduces a dynamic dispatch mechanism that inspects each invocation to determine whether the communication is local or remote. It does so by:
- Allowing the component handler to track all active components via a registry shared with the host.
- Using a unified interface with custom transport wrappers to switch between:
  - NATS-based message passing
  - In-memory streaming using `tokio::io::DuplexStream` or `tokio::sync::mpsc`

To support local invocation, the runtime invokes a new method, `Instance::call`, added to the execution pipeline. This method is responsible for:

- Initializing Wasmtime `Store` for Execution Context
- Instantiating the low-level target WebAssembly component using Wasmtime.
- Resolving the specified exported function.
- retrievinhg the function’s parameter and result types,
- Collecting guest resource for proper resource handling
- Streaming invocation parameters and returning results using trait-compliant in-memory channels (`AsyncRead`/`AsyncWrite`).

`Instance::call` serves as the short-circuit path for optimized in-host communication. When the dispatch layer determines that both caller and callee are co-located, this method bypasses message serialization and routing through NATS, offering a performance-optimized, direct invocation flow while preserving wasmCloud’s asynchronous streaming semantics.


## 📂 Repository Structure

- `main`: Branch using `DuplexStream` for optimized in-host invocation.
- `feat/nats_bypassed_implementation_with_mpsc`: Branch using `mpsc` channels as an alternative.

Note: the use of DuplexStream introduces a subtle caveat regarding stream termination. Specifically, although the caller can explicitly invoke `shutdown` on its writer after transmitting parameters, the callee (which writes the result) does not explicitly terminate the stream. As a result, the caller’s reader may block or hang if it waits for an EOF that never arrives. This behavior can surface under load, especially when buffering or scheduling delays amplify timing mismatches. In practice, this issue can lead to occasional hangs under load.



