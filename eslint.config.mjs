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
}, {
    files: ["src/opencode_plugin.js"],
    ...js.configs.all,
    languageOptions: {
        sourceType: "module",
        globals: {process: "readonly"}
    },
    rules: {
        ...js.configs.all.rules,
        "id-length": ["error", {exceptions: ["$"]}],
        camelcase: ["error", {properties: "never"}],
        "require-await": "off", // OpenCode awaits a plugin's entry point, which may have nothing to await.
        "capitalized-comments": "off", // Comment casing is style.
        "func-style": "off", // Declaration versus expression is style.
        "prefer-destructuring": "off", // Destructuring a single property is style.
        "no-magic-numbers": "off", // An argv position and a zero default are not tunable values.
        "one-var": "off", // Separate declarations are style.
        "no-inline-comments": "off", // Comment placement is style.
        "sort-keys": "off" // Hook order follows the event lifecycle, not the alphabet.
    }
}];
