.. list-table:: Configured machines
   :header-rows: 1

   * - Machine
     - Transport
     - Tags
     - Backends
     - GPUs
     - Jobs / workers
     - Subfolders
   * - Node 1
     - local
     - none
     - claude
     - none
     - 2 / 4
     - yes
   * - Node 2
     - ssh
     - cuda, linux, rtx4000
     - claude
     - 2 × Quadro RTX 4000
     - 2 / 4
     - yes
   * - Node 3
     - ssh
     - cuda, linux, rtx5090
     - claude
     - 2 × GeForce RTX 5090
     - 2 / 4
     - yes
