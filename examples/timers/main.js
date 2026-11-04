// Timers and the event bus, in one example.
//
// `start` arms a recurring host timer; the host calls `onEvent` with
// `{ topic: "timer", payload: { id } }` for each tick, and this extension
// republishes a domain event that other extensions can subscribe to.
const state = { fired: 0, remaining: 0, timerId: null };

Origin.define({
  id: "timers",
  version: "0.1.0",
  capabilities: ["log.write", "timer.schedule", "events.emit"],

  methods: {
    start(params) {
      const intervalMs = Number(params?.intervalMs ?? 100);
      const ticks = Number(params?.ticks ?? 3);
      if (state.timerId) {
        Origin.host.timers.clear(state.timerId);
      }
      state.fired = 0;
      state.remaining = ticks;
      state.timerId = Origin.host.timers.set(intervalMs, { repeat: true }).id;
      return { timerId: state.timerId, remaining: state.remaining };
    },

    stop() {
      if (state.timerId) {
        Origin.host.timers.clear(state.timerId);
        state.timerId = null;
      }
      return { fired: state.fired };
    },

    stats() {
      return { fired: state.fired, remaining: state.remaining, timerId: state.timerId };
    },
  },

  onEvent(event) {
    if (event?.topic !== "timer") return;
    state.fired += 1;
    state.remaining -= 1;
    Origin.host.events.emit("timer.tick", { fired: state.fired });
    if (state.remaining <= 0 && state.timerId) {
      Origin.host.timers.clear(state.timerId);
      state.timerId = null;
      Origin.host.log(`timer finished after ${state.fired} ticks`, "info");
    }
  },
});
