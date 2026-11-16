# Worker Diagram
This inherited diagram describes the original Narwhal architecture. This branch's worker seals/stores batches and sends their contents to its primary; the normal path has no worker-to-worker ACK quorum. See [the current GRBC flow](../README-GRBC.md#broadcast-and-storage).

<p align="center">
  <img src="https://github.com/asonnino/narwhal/blob/master/.assets/diagram-worker.svg">
</p>
