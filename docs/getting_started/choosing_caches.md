# Choosing caches

Use only the caches your workflow needs. Each one removes a different kind of repeated work:

| Repeated work | Cache that removes it |
| --- | --- |
| One node re-reads the same source content or regenerates derived data across sessions | Local cache (Hub) |
| Many clients or nodes read the same source content from S3, Azure Blob Storage, GCS, or Nucleus | Shared content cache (OVCC) |
| Many workers or GPUs generate the same derived data | Shared derived-data cache (OVDC) |

## Recommendations

- **Always use at least the local cache.** Run Hub on every node that runs a Kit-based application or Client Library tool, so end users get optimal performance. It caches derived data as well as source content on that node.
- **Deploy shared caches where the compute is.** Place OVCC and OVDC in the same region and cluster as the workers, so workers share data without each managing independent storage devices. Use one deployment of each per region the compute spans.
- **Both shared caches need Kubernetes and Helm.** Without them, use the local cache only.

## Examples

| Workflow | Caches |
| --- | --- |
| Local simulation on one workstation | Local cache |
| Kit application shipped to end users | Local cache on each user's machine |
| Simulation on 15 AWS machines in one region, reading from S3 and rendering the same scenes | Local cache on each machine, plus one OVCC and one OVDC deployment in that region |
| The same simulation split across two regions | Local cache on each machine, plus one OVCC and one OVDC deployment in each region |
| Many nodes reading shared content, each generating unique derived data | Local cache and OVCC |

For sizing, replica behavior, and edge cases, the {src}`usd-caches-expert skill <skills/usd-caches-expert/references/choosing-caches.md>` covers the full decision guide.
