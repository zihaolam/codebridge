package notify

import "testing"

func TestApplescriptStringEscaping(t *testing.T) {
	cases := map[string]string{
		`hello`:             `"hello"`,
		`say "hi"`:          `"say \"hi\""`,
		`back\slash`:        `"back\\slash"`,
		`both " and \ here`: `"both \" and \\ here"`,
	}
	for in, want := range cases {
		if got := applescriptString(in); got != want {
			t.Errorf("applescriptString(%q) = %q, want %q", in, got, want)
		}
	}
}
