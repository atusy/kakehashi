local cwd = vim.uv.cwd()

-- [[LSP]]
vim.lsp.config["kakehashi"] = {
	cmd = { cwd .. "/target/debug/kakehashi" },
	init_options = {
		languages = {
			markdown = {
				bridge = {
					python = { enabled = true },
					rust = { enabled = true },
					lua = { enabled = true },
				},
			},
		},
		languageServers = {
			["rust-analyzer"] = {
				cmd = { "rust-analyzer" },
				languages = { "rust" },
			},
			["pyright"] = {
				cmd = { "pyright-langserver", "--stdio" },
				languages = { "python" },
			},
			["lua-language-server"] = {
				cmd = { "lua-language-server" },
				languages = { "lua" },
			},
		},
	},
	on_init = function(client)
		-- to use semanticTokens/full/delta
		client.server_capabilities.semanticTokensProvider.range = false
	end,
}
vim.lsp.enable("kakehashi")
vim.lsp.log.set_level(vim.lsp.log_levels.DEBUG)

-- [[Disable Built-in Syntax Highlighting]]
vim.cmd("syntax off")
vim.api.nvim_create_autocmd("LspTokenUpdate", {
	callback = function()
		vim.treesitter.stop()
	end,
})

--[[Plugins]]
vim.pack.add({ "https://github.com/catppuccin/nvim" })
vim.cmd("set rtp+=deps/nvim/catppuccin")
require("catppuccin").setup()
vim.cmd("colorscheme catppuccin")
