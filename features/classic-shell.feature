Feature: Classic nested shell acceptance

  Scenario Outline: The nested shell reports its configured dimensions and output
    Given a nested prismattyc shell of <cols> columns and <rows> rows
    When I type "echo <marker>" and press Enter
    Then the screen shows "PT-APS-MARKER"
    Then the terminal reports 80 columns and 24 rows
    When I type "exit" and press Enter

    Examples:
      | cols | rows | marker         |
      | 80   | 24   | PT-APS-MARKER |
