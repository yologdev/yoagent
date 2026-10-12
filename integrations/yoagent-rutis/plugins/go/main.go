// A yoagent handler written in Go, with the rutis Go SDK.
//
// It registers a handler — a map of Go functions — with the `yoagent` host
// service: a policy (`before_tool`), a redaction (`after_tool`) and tools
// (`tools`, `call_tool`). Each hook gets one argument, an object decoded
// into a map; its `signal` field decodes as nil: the host's cancel arrives
// as the hook's context.Context instead, cancelled when the host gives the
// call up (the run cancelled, a hook timed out, the plugin unloaded). As Go
// contexts do, it also ends when the hook returns: watch it only while the
// call runs.
// Returning an error fails closed, as a throw does in TypeScript: a policy
// denies, a redaction withholds, a tool call fails.
//
// tests/go_test.rs builds and runs it; `config.out` is a file it writes
// what happened to.
package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/arcships/rutis/go/rutis"
)

type Config struct {
	Out string `json:"out"`
}

// record appends `line` to the file `path`; a failure goes to stderr, which
// reaches the host's.
func record(path, line string) {
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err == nil {
		_, err = fmt.Fprintln(f, line)
		if closeErr := f.Close(); err == nil {
			err = closeErr
		}
	}
	if err != nil {
		fmt.Fprintf(os.Stderr, "go-policy: record %q to %s: %v\n", line, path, err)
	}
}

var Plugin = rutis.Define(rutis.Plugin[Config]{
	Name:   "go-policy",
	Inject: []string{"yoagent"},
	Apply: func(ctx *rutis.Ctx, config Config) error {
		var yoagent rutis.Service
		if err := ctx.Use("yoagent", &yoagent); err != nil {
			return err
		}
		handler := map[string]any{
			"before_tool": func(call context.Context, c map[string]any) (any, error) {
				tool, _ := c["tool"].(string)
				record(config.Out, "go before_tool: "+tool)
				switch tool {
				case "bash":
					return map[string]any{"deny": "no shell from Go"}, nil
				case "fail_gate":
					return nil, errors.New("go policy backend down")
				case "odd_gate":
					return map[string]any{"allow": "maybe"}, nil
				case "slow_gate":
					select {
					case <-call.Done():
						record(config.Out, "go slow_gate: cancelled")
						return nil, call.Err()
					case <-time.After(60 * time.Second):
						return nil, nil
					}
				}
				return nil, nil
			},
			"after_tool": func(_ context.Context, c map[string]any, output map[string]any) (any, error) {
				if c["tool"] == "go_echo" {
					text, _ := output["text"].(string)
					return map[string]any{"text": strings.ReplaceAll(text, "secret", "[go redacted]")}, nil
				}
				return nil, nil
			},
			"tools": func(context.Context, map[string]any) (any, error) {
				return []map[string]any{
					{"name": "go_echo", "description": "echoes its arguments from Go"},
					{"name": "go_wait", "description": "waits until cancelled"},
				}, nil
			},
			"call_tool": func(call context.Context, c map[string]any) (any, error) {
				switch c["tool"] {
				case "go_wait":
					record(config.Out, "go wait: started")
					<-call.Done()
					record(config.Out, "go wait: cancelled")
					return nil, call.Err()
				}
				return fmt.Sprintf("go says %v", c["args"]), nil
			},
		}
		var dispose *rutis.Function
		if err := yoagent.Call(ctx, "register", &dispose, "go-policy", handler); err != nil {
			return err
		}
		if dispose == nil {
			return errors.New("register returned no disposer")
		}
		record(config.Out, "go registered")
		// The handler goes when the plugin unloads.
		ctx.Effect(func(c context.Context) error { return dispose.Call(c, nil) })
		return nil
	},
})

func main() { rutis.Serve(Plugin) }
