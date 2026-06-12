import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:local_focus_mobile/main.dart';

void main() {
  testWidgets('Local Focus shell shows the boot screen first', (WidgetTester tester) async {
    await tester.pumpWidget(const LocalFocusApp());
    expect(find.byType(MaterialApp), findsOneWidget);
    // Before the embedded server is ready the boot screen shows a spinner.
    expect(find.byType(CircularProgressIndicator), findsOneWidget);
  });
}
