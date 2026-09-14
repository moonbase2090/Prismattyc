Feature: Classic terminal colour acceptance

  Scenario Outline: The nested shell preserves the requested foreground sequence
    Given a nested prismattyc shell of 80 columns and 16 rows
    When I type "printf '\033[<fg>m<text>\033[0m <fg>\n'" and press Enter
    Then the screen shows "hello world 38;2;255;0;0"
    When I type "exit" and press Enter

    Examples:
      | text        | fg             |
      | hello world | 38;2;255;0;0  |
