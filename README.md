# scribe
A tiny, offline terminal explorer for Kubernetes object authorship. It decodes an object's managedFields into a browsable field → actor → timestamp graph, so you can see exactly which controller last wrote each field and pivot across the owner chain. 


## Usage

```sh
scribe                          # discover the cluster, open the overview (namespaces)
scribe --context prod           # pick a kubeconfig context
scribe --kubeconfig ./kc.yaml   # pick a kubeconfig file
scribe --no-tui                 # plain-text overview (exit code 2 if no cluster / unreachable)
```

Discovery order: `--kubeconfig`, `$KUBECONFIG` (a list), `~/.kube/config`, then the in-cluster
service account. Only non-secret structure is read (context, cluster, server, user *name*,
namespace); credentials are never parsed. For now scribe talks to the cluster through the
`kubectl` binary on `PATH`, so every auth plugin (EKS, OIDC, ...) works. If nothing is found the
TUI still opens with a placeholder and a warning.

Offline, from saved or piped objects (no cluster needed):

```sh
# kubectl hides managedFields by default; ask for them
kubectl get deploy,rs,pod -n prod -o yaml --show-managed-fields | scribe     # TUI on a terminal
scribe objects.yaml -f 'spec.replicas'            # non-interactive table
scribe objects.yaml -o json --manager kubectl     # for scripts
scribe objects.yaml -o tsv --since 2h             # object, field, writer, op, time, age
```

Empty stdin (cron, CI, `</dev/null`) counts as "no input" and falls back to cluster discovery.

Load several objects (a `List`, multi-doc YAML, or several files) to follow the owner chain
Pod → ReplicaSet → Deployment. Output is automatic: TUI when stdout is a terminal, plain text
otherwise (`--no-tui` forces it).

Flow: **namespaces → objects → fields**. `enter` on a namespace loads its workloads
(Deployments, StatefulSets, DaemonSets, ReplicaSets, Jobs, CronJobs, Pods, Services; never
Secrets) with their managedFields. The object picker shows the owner tree with each object's
latest writer and age; `enter` opens the field → actor → timestamp view, where `o`/`c` pivot up
and down the owner chain. `esc` always steps back one level (fields → objects → namespaces → quit).

Keys, overview: `j/k` move · `g/G` ends · `enter` explore · `r` reload · `q` quit.
Keys, objects: `j/k` move · `/` find · `enter` open · `esc` back.
Keys, fields: `j/k` move · `/` find · `m` sort by manager · `M` pivot on the selected writer ·
`o` owner · `c` child · `b` back · `p` object picker · `x` clear filters · `esc` back.

"Last writer" = the managedFields entry with the newest `time` that owns the field; other
owners are listed as co-owners (`+N`). Timestamps are the API server's own, so they show when
a manager last *touched* its entry, not necessarily when the value last changed.

## Build

```sh
cargo build --release   # ~1 MB binary
cargo test
```
