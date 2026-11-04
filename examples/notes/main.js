// Storage is a byte store behind a capability check. Keys live in a namespace
// owned by this extension; `notes/` here is a convention inside that namespace,
// not a security boundary.
Origin.define({
  id: "notes",
  version: "0.1.0",
  capabilities: ["storage.read", "storage.write"],

  methods: {
    save(params) {
      const key = String(params?.key ?? "");
      if (!key) throw new Error("key is required");
      Origin.host.storage.set(`notes/${key}`, String(params?.value ?? ""));
      return { saved: key };
    },

    read(params) {
      return { value: Origin.host.storage.get(`notes/${params?.key ?? ""}`) };
    },

    remove(params) {
      Origin.host.storage.remove(`notes/${params?.key ?? ""}`);
      return { removed: true };
    },

    list() {
      return {
        notes: Origin.host.storage.list("notes/").map((entry) => ({
          key: entry.key.slice("notes/".length),
          value: entry.value,
        })),
      };
    },
  },
});
