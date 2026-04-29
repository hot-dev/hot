# node-hello - `::hot::box/start` mounts smoke test

Minimal Node app used by `frontpage-demos/box-mounts.hot` so it can be
bind-mounted into a container at runtime via `::hot::box/start`.

```text
hot/src-offline/frontpage-demos/resources/box-demos/node-hello/
├── server.js        # zero-dependency Node script
└── README.md        # this file
```

To run the offline demo, configure this directory as a Hot resource root:

```hot
hot.project.<project>.resources.paths [
    "./hot/src-offline/frontpage-demos/resources/box-demos",
]
```

With that resource root, `node-hello/server.js` becomes the bundle-relative
resource mounted by `mounts: {"/app": "node-hello"}`.
