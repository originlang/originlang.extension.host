// Preloaded before extension code. Everything an extension can reach is frozen
// into `globalThis.Origin`; no host object is exposed directly, so the only way
// out of the sandbox is the JSON string callback registered by Rust.
(() => {
  let definition = null;
  let activated = false;

  const invokeHost = (method, params) => {
    const reply = JSON.parse(__oextHost(method, JSON.stringify(params ?? null)));
    if (reply.error) {
      const error = new Error(reply.error.message);
      error.code = reply.error.code;
      throw error;
    }
    return reply.result;
  };

  const storage = Object.freeze({
    get: (key) => invokeHost("host.storage.get", { key: String(key) }),
    set: (key, value) => invokeHost("host.storage.set", { key: String(key), value: String(value) }),
    remove: (key) => invokeHost("host.storage.delete", { key: String(key) }),
    list: (prefix = "") => invokeHost("host.storage.list", { prefix: String(prefix) }),
  });

  const timers = Object.freeze({
    set: (delayMs, options) =>
      invokeHost("host.timer.set", {
        delayMs: Number(delayMs),
        repeat: options?.repeat === true,
      }),
    clear: (id) => invokeHost("host.timer.clear", { id: String(id) }),
  });

  const events = Object.freeze({
    emit: (topic, payload) => invokeHost("host.events.emit", { topic: String(topic), payload }),
    subscribe: (topic) => invokeHost("host.events.subscribe", { topic: String(topic) }),
  });

  const host = Object.freeze({
    call: invokeHost,
    log: (message, level = "info") =>
      invokeHost("host.log", { message: String(message), level: String(level) }),
    info: () => invokeHost("host.self.info", null),
    callExtension: (id, method, params) =>
      invokeHost("host.extension.call", { id: String(id), method: String(method), params }),
    storage,
    timers,
    events,
  });

  globalThis.Origin = Object.freeze({
    apiVersion: "1",
    host,
    define(value) {
      if (definition) throw new Error("extension already defined");
      if (!value || typeof value.id !== "string" || typeof value.version !== "string") {
        throw new Error("extension definition requires id and version");
      }
      definition = value;
    },
  });

  const current = () => {
    if (!definition) throw new Error("extension did not call Origin.define");
    return definition;
  };

  const activate = async () => {
    if (activated) return;
    activated = true;
    await definition.activate?.(host);
  };

  globalThis.__oextInvoke = async (method, payload) => {
    try {
      const extension = current();
      const params = JSON.parse(payload);
      let result;
      if (method === "extension.register") {
        result = {
          id: extension.id,
          version: extension.version,
          capabilities: extension.capabilities ?? [],
          methods: Object.keys(extension.methods ?? {}),
        };
      } else if (method === "extension.ping") {
        result = { pong: true, id: extension.id };
      } else if (method === "extension.activate") {
        await activate();
        result = null;
      } else if (method === "extension.shutdown" || method === "extension.deactivate") {
        result = (await extension.deactivate?.()) ?? null;
      } else if (method === "extension.event") {
        await activate();
        result = (await extension.onEvent?.(params, host)) ?? null;
      } else {
        await activate();
        const handlers = extension.methods ?? {};
        const handler = Object.prototype.hasOwnProperty.call(handlers, method)
          ? handlers[method]
          : undefined;
        if (typeof handler !== "function") {
          const error = new Error(`method not found: ${method}`);
          error.code = -32601;
          throw error;
        }
        result = await handler(params, host);
      }
      return JSON.stringify({ result: result ?? null });
    } catch (error) {
      return JSON.stringify({
        error: {
          code: Number.isInteger(error?.code) ? error.code : -32005,
          message: String(error?.message ?? error),
        },
      });
    }
  };
})();
