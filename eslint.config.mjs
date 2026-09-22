import js from "@eslint/js";

export default [{
    files: ["src/adapters/ghostty_first_terminal.js"],
    ...js.configs.all,
    languageOptions: {
        sourceType: "script",
        globals: {ObjC: "readonly", $: "readonly", Ref: "readonly"}
    },
    rules: {
        ...js.configs.all.rules,
        strict: ["error", "global"],
        "new-cap": ["error", {capIsNewExceptions: ["Ref"]}],
        "one-var": "off", // Separate declarations keep native event construction readable.
        "no-magic-numbers": "off", // Apple event descriptor codes are protocol constants.
        "no-bitwise": "off", // Apple event options are bit flags.
        "max-params": "off", // Native object specifiers have four parts.
        "max-statements": "off" // Splitting one native request would obscure its order.
    }
}];
