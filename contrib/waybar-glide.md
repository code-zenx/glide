# waybar glide indicator

Shows glide link state in waybar. `glide status --json --watch` emits one waybar JSON
object per second; waybar applies the `class` field (`local`, `sending`, `receiving`,
`offline`) to the module widget.

## Module (`~/.config/waybar/config.jsonc`)

Add `"custom/glide"` to `modules-right` (placed before `"network"`, next to the other
connectivity modules) and define it:

```jsonc
  "custom/glide": {
    "exec": "/home/sudomrx/projects/glide/target/release/glide status --json --watch",
    "return-type": "json",
    "tooltip": true
  },
```

No `interval` — `--watch` is a long-running process, waybar reads its stdout line by line.
Adjust the binary path per machine.

## Style (`~/.config/waybar/style.css`)

Catppuccin Mocha colours. GTK CSS: no custom properties, plain hex only.

```css
/* glide — keyboard/mouse share link; class comes from `glide status --json` */
#custom-glide {
  padding: 0 8px;
  margin: 0;
  color: #cdd6f4;
  font-size: 12px;
  border-left: 1px solid #313244;
}

#custom-glide label {
  font-family: "FiraCode Nerd Font Propo", sans-serif;
  min-width: 6rem;
}

#custom-glide.local {
  color: #cdd6f4;
}

#custom-glide.sending {
  color: #f9e2af;
}

#custom-glide.receiving {
  color: #89dceb;
}

#custom-glide.offline {
  color: #6c7086;
}
```

`min-width` on the label stops the bar jittering as the text changes between
`✕ offline`, `● 1.8ms` and `→ goldengate`.

## Reload

```sh
bash ~/.config/waybar/reload.sh --reload
```

Verify: `pgrep -a -P $(pgrep -x waybar)` should list the `glide status --json --watch`
child, and the waybar log (`~/.cache/waybar-reload.log`) should contain no JSON parse
errors.
