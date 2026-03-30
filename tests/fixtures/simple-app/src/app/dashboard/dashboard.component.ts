import { Component } from '@angular/core';
import { SharedService } from '../shared/shared.service';

@Component({
  selector: 'app-dashboard',
  standalone: true,
  template: '<h2>Dashboard: {{ items }}</h2>'
})
export class DashboardComponent {
  items = SharedService.getData().length;
}
