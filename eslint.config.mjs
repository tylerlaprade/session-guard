import js from "@eslint/js";

export default [{
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
