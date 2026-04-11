package config

import (
	"reflect"
	"testing"
)

func TestNormalizeBoolCLIArgs(t *testing.T) {
	t.Parallel()
	tests := []struct {
		name string
		in   []string
		want []string
	}{
		{
			name: "merge_false_before_other_flags",
			in:   []string{"prog", "-auto-flatten", "false", "-spread", "0.002"},
			want: []string{"prog", "-auto-flatten=false", "-spread", "0.002"},
		},
		{
			name: "merge_true_double_dash_name",
			in:   []string{"prog", "--dry-run", "true", "-network", "mainnet"},
			want: []string{"prog", "--dry-run=true", "-network", "mainnet"},
		},
		{
			name: "already_equals_form_unchanged",
			in:   []string{"prog", "-auto-flatten=false", "-spread", "0.002"},
			want: []string{"prog", "-auto-flatten=false", "-spread", "0.002"},
		},
		{
			name: "bool_followed_by_flag_not_merged",
			in:   []string{"prog", "-auto-flatten", "-spread", "0.002"},
			want: []string{"prog", "-auto-flatten", "-spread", "0.002"},
		},
		{
			name: "non_bool_next_token_not_merged",
			in:   []string{"prog", "-auto-flatten", "0.002", "-spread", "0.001"},
			want: []string{"prog", "-auto-flatten", "0.002", "-spread", "0.001"},
		},
		{
			name: "after_double_dash_no_merge",
			in:   []string{"prog", "--", "-auto-flatten", "false", "-spread", "0.002"},
			want: []string{"prog", "--", "-auto-flatten", "false", "-spread", "0.002"},
		},
		{
			name: "yes_no_tokens",
			in:   []string{"prog", "-tg-alert-inventory", "no", "-spread", "0.1"},
			want: []string{"prog", "-tg-alert-inventory=no", "-spread", "0.1"},
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Parallel()
			got := normalizeBoolCLIArgs(tt.in)
			if !reflect.DeepEqual(got, tt.want) {
				t.Fatalf("normalizeBoolCLIArgs() = %#v, want %#v", got, tt.want)
			}
		})
	}
}
