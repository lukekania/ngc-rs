import { Component } from '@angular/core';
import { SharedService } from '../shared/shared.service';

@Component({
  selector: 'app-admin',
  standalone: true,
  template: '<h2>Admin: {{ data }}</h2>'
})
export class AdminComponent {
  data = SharedService.getData().join(', ');
}
