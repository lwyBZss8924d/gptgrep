# DevBox Architecture Specification

## 1. Infrastructure Topology (Pattern A)

The GPTgrep DevBox infrastructure utilizes a single dedicated Google Compute Engine (GCE) virtual machine optimized for high-throughput compilation, isolated container verification, and deep retrieval benchmarking.

### 1.1 Compute & Network
- **Instance ID**: `gptgrep-devbox-01`
- **Machine Type**: `n2-standard-16` (16 vCPU, 64 GiB physical RAM)
- **Zone / Region**: `us-east4-a` (`us-east4`)
- **OS**: Ubuntu 24.04 LTS (Kernel 6.8+ GCP optimized)
- **Virtualization**: Nested virtualization enabled (`--enable-nested-virtualization`) for hardware-accelerated Docker/KVM isolation.
- **Network Ingress**: Zero open public ingress. SSH is strictly mediated through Google Cloud Identity-Aware Proxy (IAP) CIDR `35.235.240.0/20` using OS Login.

### 1.2 Storage Architecture
- **Root Boot Disk**: 50 GB standard persistent disk (`pd-standard`). Dedicated to ephemeral OS state.
- **Dedicated Data Volume**: 250 GB balanced persistent disk (`pd-balanced`), formatted as `ext4` with discard and mounted at `/mnt/gptgrep-dev` (`auto-delete=no`).
- **Cold Storage Tier**: Google Cloud Storage bucket `gs://gptgrep-xl-docbench-gws-personal-20261016` located in region `US-EAST4` for zero-egress archive streaming.
- **Docker Engine Storage**: Daemon `data-root` relocated to `/mnt/gptgrep-dev/docker-data` with `overlay2` storage driver and `live-restore: true`.
