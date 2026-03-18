;;; minimal_init.el --- Minimal Emacs configuration for kakehashi LSP testing -*- lexical-binding: t -*-

;;; Commentary:
;; This configuration mirrors scripts/minimal_init.lua for Neovim.
;; It sets up lsp-mode with kakehashi as the language server.

;;; Code:

;; Disable package.el at startup
(setq package-enable-at-startup nil)

;; Get current working directory
(defvar kakehashi--cwd (or (getenv "PWD") default-directory))

;; Add lsp-mode and dependencies to load-path
(add-to-list 'load-path (expand-file-name "deps/emacs/dash.el" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/f.el" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/s.el" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/ht.el" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/spinner.el" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/markdown-mode" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/hydra" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/lsp-mode" kakehashi--cwd))
(add-to-list 'load-path (expand-file-name "deps/emacs/lsp-mode/clients" kakehashi--cwd))

;; Required libraries
(require 'lsp-mode)
(require 'lsp-completion)

;; Disable optional lsp-mode features not needed for testing
(setq lsp-lens-enable nil)

;; Map file extensions to LSP language IDs (needed when major modes aren't installed)
(add-to-list 'lsp-language-id-configuration '("\\.lua\\'" . "lua"))
(add-to-list 'lsp-language-id-configuration '("\\.py\\'" . "python"))
(add-to-list 'lsp-language-id-configuration '("\\.rs\\'" . "rust"))
(add-to-list 'lsp-language-id-configuration '("\\.md\\'" . "markdown"))

;; LSP performance settings
(setq gc-cons-threshold 100000000)
(setq read-process-output-max (* 1024 1024))

;; LSP logging (mirrors vim.lsp.log.set_level DEBUG)
(setq lsp-log-io t)

;; Register kakehashi as a language server
(lsp-register-client
 (make-lsp-client
  :new-connection (lsp-stdio-connection
                   (lambda ()
                     (list (expand-file-name "target/debug/kakehashi" kakehashi--cwd))))
  :major-modes nil
  :activation-fn (lambda (&rest _) t)
  :server-id 'kakehashi
  :priority 1
  :initialization-options
  (lambda ()
    '(:languages
      (:markdown
       (:bridge
        (:python (:enabled t)
         :rust (:enabled t)
         :lua (:enabled t))))
      :languageServers
      (:rust-analyzer
       (:cmd ["rust-analyzer"]
        :languages ["rust"])
       :pyright
       (:cmd ["pyright-langserver" "--stdio"]
        :languages ["python"])
       :lua-language-server
       (:cmd ["lua-language-server"]
        :languages ["lua"]))))))

;; Enable semantic tokens
(setq lsp-semantic-tokens-enable t)

;; Disable native syntax highlighting but keep font-lock infrastructure for LSP semantic tokens
;; Setting empty keywords means no native highlighting, but font-lock can still apply LSP faces
(setq-default font-lock-defaults '(nil t))
(global-font-lock-mode 1)


;; Start LSP for any file-visiting buffer
(add-hook 'find-file-hook #'lsp)

(add-hook 'lsp-after-open-hook #'lsp-semantic-tokens-mode)

(provide 'minimal_init)
