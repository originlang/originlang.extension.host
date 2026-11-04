// Minimal extension. It demonstrates the three things every extension does:
// declare itself, expose methods, and react to events.
Origin.define({
  id: "hello",
  version: "0.1.0",
  capabilities: ["log.write"],

  activate() {
    Origin.host.log("hello activated", "debug");
  },

  methods: {
    greet(params) {
      return {
        message: `Hello, ${params?.name ?? "world"}`,
        api: Origin.host.info().apiVersion,
      };
    },

    // Requires `log.write`. Without the grant this throws PERMISSION_DENIED,
    // which the host surfaces unchanged to the caller.
    announce(params) {
      Origin.host.log(params?.message ?? "hello", "info");
      return { announced: true };
    },
  },

  onEvent(event) {
    Origin.host.log(`event: ${event.topic}`, "debug");
  },
});
