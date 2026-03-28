import { Component } from '@angular/core';
import { RouterOutlet } from '@angular/router';
import { SharedUtils } from '@app/shared';

@Component({
  selector: 'app-root',
  standalone: true,
  imports: [RouterOutlet],
  template: '<h1>{{ title }}</h1><router-outlet />'
})
export class AppComponent {
  title = SharedUtils.appName();
}
